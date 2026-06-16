use super::*;

const MAX_CONTENT_MATCHES_PER_FILE: usize = 20;

pub(super) struct ContentSearchInput<'a, P> {
    pub(super) provider: &'a P,
    pub(super) snapshot: &'a CatalogSnapshot,
    pub(super) request: &'a SearchRequest,
    pub(super) filter: &'a EntryFilter,
    pub(super) regex: Option<&'a Regex>,
    pub(super) ac: Option<&'a AhoCorasick>,
    pub(super) case_sensitive: bool,
    pub(super) max_results: usize,
}

pub(super) struct ContentSearchSummary {
    pub(super) matches: Vec<ContentSearchMatch>,
    pub(super) match_files: Vec<FileMatchCount>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) total: usize,
    pub(super) files_scanned: usize,
    pub(super) bytes_scanned: u64,
    pub(super) binary_files_skipped: usize,
    pub(super) exhausted: bool,
}

pub(super) fn collect_content_matches<P: CatalogProvider + Sync>(
    input: ContentSearchInput<'_, P>,
    cancel: &CancelToken,
) -> Result<ContentSearchSummary, CtxError> {
    let mut summary = ContentSearchSummary::empty();
    if !matches!(input.request.mode, SearchMode::Content | SearchMode::Both)
        || input.request.pattern.is_empty()
    {
        return Ok(summary);
    }

    let ranking_query = ContentRankingQuery::new(
        &input.request.pattern,
        input.case_sensitive,
        input.request.regex,
        input.request.whole_word,
    );
    let (content_entries, exhausted) =
        plan_content_entries(input.snapshot, input.request, input.filter, cancel)?;
    summary.exhausted = exhausted;

    let files_scanned = AtomicUsize::new(0);
    let bytes_scanned = AtomicU64::new(0);
    let binary_skipped = AtomicUsize::new(0);
    let results = content_search_results(
        &input,
        &content_entries,
        &ranking_query,
        &files_scanned,
        &bytes_scanned,
        &binary_skipped,
        cancel,
    );
    let mut ranking_stats = Vec::new();
    for result in results {
        let found = result?;
        summary.total += found.matches.len();
        summary.matches.extend(found.matches);
        if let Some(stats) = found.ranking {
            ranking_stats.push(stats);
        }
        if let Some(diagnostic) = found.diagnostic {
            summary.diagnostics.push(diagnostic);
        }
    }

    shape_content_output(
        &mut summary,
        input.request,
        input.max_results,
        &ranking_stats,
        &ranking_query,
        cancel,
    )?;
    summary.files_scanned = files_scanned.load(Ordering::Relaxed);
    summary.bytes_scanned = bytes_scanned.load(Ordering::Relaxed);
    summary.binary_files_skipped = binary_skipped.load(Ordering::Relaxed);
    Ok(summary)
}

impl ContentSearchSummary {
    fn empty() -> Self {
        Self {
            matches: Vec::new(),
            match_files: Vec::new(),
            diagnostics: Vec::new(),
            total: 0,
            files_scanned: 0,
            bytes_scanned: 0,
            binary_files_skipped: 0,
            exhausted: false,
        }
    }
}

pub(super) fn plan_content_entries<'a>(
    snapshot: &'a CatalogSnapshot,
    request: &SearchRequest,
    filter: &EntryFilter,
    cancel: &CancelToken,
) -> Result<(Vec<&'a CatalogEntry>, bool), CtxError> {
    let mut planned_bytes = 0u64;
    let mut content_entries = Vec::new();
    for entry in &snapshot.entries {
        cancel.check_cancelled()?;
        if !filter.accepts(&entry.rel_path) {
            continue;
        }
        if content_entries.len() >= request.max_content_files
            || planned_bytes.saturating_add(entry.size) > request.max_content_bytes
        {
            return Ok((content_entries, true));
        }
        planned_bytes = planned_bytes.saturating_add(entry.size);
        content_entries.push(entry);
    }
    Ok((content_entries, false))
}

pub(super) fn content_search_results<P: CatalogProvider + Sync>(
    input: &ContentSearchInput<'_, P>,
    entries: &[&CatalogEntry],
    ranking_query: &ContentRankingQuery,
    files_scanned: &AtomicUsize,
    bytes_scanned: &AtomicU64,
    binary_skipped: &AtomicUsize,
    cancel: &CancelToken,
) -> Vec<Result<ContentSearchResult, CtxError>> {
    #[cfg(not(target_arch = "wasm32"))]
    let iter = entries.par_iter();
    #[cfg(target_arch = "wasm32")]
    let iter = entries.iter();

    iter.map(|entry| {
        content_search_file(
            input,
            entry,
            ranking_query,
            files_scanned,
            bytes_scanned,
            binary_skipped,
            cancel,
        )
    })
    .collect()
}

pub(super) fn content_search_file<P: CatalogProvider + Sync>(
    input: &ContentSearchInput<'_, P>,
    entry: &CatalogEntry,
    ranking_query: &ContentRankingQuery,
    files_scanned: &AtomicUsize,
    bytes_scanned: &AtomicU64,
    binary_skipped: &AtomicUsize,
    cancel: &CancelToken,
) -> Result<ContentSearchResult, CtxError> {
    cancel.check_cancelled()?;
    let bytes = input.provider.read_bytes(Path::new(&entry.abs_path))?;
    cancel.check_cancelled()?;
    files_scanned.fetch_add(1, Ordering::Relaxed);
    bytes_scanned.fetch_add(bytes.len() as u64, Ordering::Relaxed);

    if is_binary(&bytes) {
        binary_skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(binary_content_result(entry));
    }

    let text = String::from_utf8_lossy(&bytes);
    let display_path = display_path(input.snapshot, &entry.root_id, &entry.rel_path);
    let context_before = input
        .request
        .context_before
        .unwrap_or(input.request.context_lines);
    let context_after = input
        .request
        .context_after
        .unwrap_or(input.request.context_lines);
    let match_set = content_matches_for_file(
        ContentMatchInput {
            text: &text,
            root_id: &entry.root_id,
            path: &entry.rel_path,
            display_path: &display_path,
            context_before,
            context_after,
            pattern: &input.request.pattern,
            regex: input.regex,
            ac: input.ac,
            whole_word: input.request.whole_word,
        },
        cancel,
    )?;
    cancel.check_cancelled()?;
    Ok(ContentSearchResult {
        ranking: Some(ranking_query.stats_for_file(
            &entry.rel_path,
            &text,
            match_set.occurrence_count,
        )),
        matches: match_set.matches,
        diagnostic: None,
    })
}

pub(super) fn binary_content_result(entry: &CatalogEntry) -> ContentSearchResult {
    ContentSearchResult {
        matches: Vec::new(),
        ranking: None,
        diagnostic: Some(Diagnostic {
            path: Some(PathBuf::from(&entry.rel_path)),
            message: format!(
                "skipped binary file during content search: {}",
                entry.rel_path
            ),
        }),
    }
}

pub(super) fn shape_content_output(
    summary: &mut ContentSearchSummary,
    request: &SearchRequest,
    max_results: usize,
    ranking_stats: &[FileRankingStats],
    ranking_query: &ContentRankingQuery,
    cancel: &CancelToken,
) -> Result<(), CtxError> {
    cancel.check_cancelled()?;
    match request.output_mode {
        OutputMode::Content => {
            apply_content_relevance_scores(&mut summary.matches, ranking_stats, ranking_query);
            summary.matches.sort_by(content_match_cmp);
            summary.matches =
                diversify_content_matches(std::mem::take(&mut summary.matches), max_results);
        }
        OutputMode::FilesWithMatches | OutputMode::Count => {
            summary.match_files = collapse_to_files(&summary.matches);
            if summary.match_files.len() > max_results {
                summary.match_files.truncate(max_results);
            }
            summary.matches = Vec::new();
        }
    }
    Ok(())
}

pub(super) fn diversify_content_matches(
    matches: Vec<ContentSearchMatch>,
    max_results: usize,
) -> Vec<ContentSearchMatch> {
    let groups = group_matches_by_ranked_file(matches, content_file_match_cap(max_results));
    let mut selected = Vec::new();
    let mut round = 0usize;
    while selected.len() < max_results {
        let mut added = false;
        for group in &groups {
            if let Some(hit) = group.get(round) {
                selected.push(hit.clone());
                added = true;
                if selected.len() >= max_results {
                    break;
                }
            }
        }
        if !added {
            break;
        }
        round += 1;
    }
    selected
}

fn content_file_match_cap(max_results: usize) -> usize {
    max_results
        .div_ceil(3)
        .clamp(1, MAX_CONTENT_MATCHES_PER_FILE)
}

fn group_matches_by_ranked_file(
    matches: Vec<ContentSearchMatch>,
    per_file_cap: usize,
) -> Vec<Vec<ContentSearchMatch>> {
    let mut groups: Vec<Vec<ContentSearchMatch>> = Vec::new();
    for hit in matches {
        if let Some(group) = groups.iter_mut().find(|group| group[0].path == hit.path) {
            if group.len() < per_file_cap {
                group.push(hit);
            }
        } else {
            groups.push(vec![hit]);
        }
    }
    groups
}

pub(super) struct ContentSearchResult {
    matches: Vec<ContentSearchMatch>,
    ranking: Option<FileRankingStats>,
    diagnostic: Option<Diagnostic>,
}

pub(super) struct ContentMatchInput<'a> {
    text: &'a str,
    root_id: &'a str,
    path: &'a str,
    display_path: &'a str,
    context_before: usize,
    context_after: usize,
    pattern: &'a str,
    regex: Option<&'a Regex>,
    ac: Option<&'a AhoCorasick>,
    whole_word: bool,
}

pub(super) struct ContentMatchSet {
    matches: Vec<ContentSearchMatch>,
    occurrence_count: usize,
}

pub(super) fn content_matches_for_file(
    input: ContentMatchInput<'_>,
    cancel: &CancelToken,
) -> Result<ContentMatchSet, CtxError> {
    let lines: Vec<&str> = input.text.lines().collect();
    let mut matches = Vec::new();
    let mut occurrence_count = 0usize;
    for (idx, line) in lines.iter().enumerate() {
        cancel.check_cancelled()?;
        let columns = find_content_match_columns(line, &input, cancel)?;
        occurrence_count += columns.len();
        if let Some(col0) = columns.first().copied() {
            let line_number = idx + 1;
            matches.push(ContentSearchMatch {
                root_id: input.root_id.to_string(),
                path: input.path.to_string(),
                display_path: input.display_path.to_string(),
                score: 0,
                line: line_number,
                column: col0 + 1,
                text: trim_preview(line),
                context: line_context(&lines, idx, input.context_before, input.context_after),
            });
        }
    }
    Ok(ContentMatchSet {
        matches,
        occurrence_count,
    })
}

pub(super) fn find_content_match_columns(
    line: &str,
    input: &ContentMatchInput<'_>,
    cancel: &CancelToken,
) -> Result<Vec<usize>, CtxError> {
    if let Some(regex) = input.regex {
        let mut columns = Vec::new();
        for mat in regex.find_iter(line) {
            cancel.check_cancelled()?;
            if !input.whole_word || is_whole_word_match(line, mat.start(), mat.end()) {
                columns.push(mat.start());
            }
        }
        return Ok(columns);
    }
    if let Some(ac) = input.ac {
        let mut columns = Vec::new();
        for mat in ac.find_iter(line) {
            cancel.check_cancelled()?;
            if !input.whole_word || is_whole_word_match(line, mat.start(), mat.end()) {
                columns.push(mat.start());
            }
        }
        return Ok(columns);
    }
    literal_match_columns(line, input.pattern, true, false, cancel)
}

pub(super) fn literal_match_columns(
    text: &str,
    pattern: &str,
    case_sensitive: bool,
    whole_word: bool,
    cancel: &CancelToken,
) -> Result<Vec<usize>, CtxError> {
    if pattern.is_empty() {
        return Ok(Vec::new());
    }
    let mut columns = Vec::new();
    let mut offset = 0usize;
    while offset <= text.len().saturating_sub(pattern.len()) {
        cancel.check_cancelled()?;
        let found = if case_sensitive {
            text[offset..].find(pattern)
        } else {
            find_ascii_case_insensitive(&text.as_bytes()[offset..], pattern.as_bytes())
        };
        let Some(relative_start) = found else {
            break;
        };
        let start = offset + relative_start;
        let end = start + pattern.len();
        if !whole_word || is_whole_word_match(text, start, end) {
            columns.push(start);
        }
        offset = end;
    }
    Ok(columns)
}

pub(super) fn apply_content_relevance_scores(
    matches: &mut [ContentSearchMatch],
    stats: &[FileRankingStats],
    query: &ContentRankingQuery,
) {
    let scores = content_file_scores(stats, query);
    for hit in matches {
        hit.score = scores.get(&hit.path).copied().unwrap_or_default();
    }
}

pub(super) fn line_context(
    lines: &[&str],
    match_idx: usize,
    context_before: usize,
    context_after: usize,
) -> Vec<LineContext> {
    let start = match_idx.saturating_sub(context_before);
    let end = (match_idx + context_after + 1).min(lines.len());
    (start..end)
        .map(|idx| LineContext {
            line: idx + 1,
            text: trim_preview(lines[idx]),
        })
        .collect()
}

pub(super) fn trim_preview(line: &str) -> String {
    line.chars().take(240).collect()
}

#[cfg(fuzzing)]
#[doc(hidden)]
pub fn fuzz_match_content(
    content: &str,
    pattern: &str,
    regex: bool,
    whole_word: bool,
) -> Result<usize, CtxError> {
    let case_sensitive = is_smart_case_sensitive(pattern);
    let compiled_regex = if regex {
        Some(
            RegexBuilder::new(pattern)
                .case_insensitive(!case_sensitive)
                .build()?,
        )
    } else {
        None
    };
    let ac = if regex || pattern.is_empty() {
        None
    } else {
        Some(
            AhoCorasickBuilder::new()
                .ascii_case_insensitive(!case_sensitive)
                .build([pattern])
                .expect("single literal pattern"),
        )
    };
    let matches = content_matches_for_file(
        ContentMatchInput {
            text: content,
            root_id: "fuzz",
            path: "fuzz.txt",
            display_path: "fuzz/fuzz.txt",
            context_before: 2,
            context_after: 2,
            pattern,
            regex: compiled_regex.as_ref(),
            ac: ac.as_ref(),
            whole_word,
        },
        &CancelToken::never(),
    )?;
    Ok(matches.occurrence_count)
}

/// Collapse content matches to one entry per file with its matched-line count,
/// ordered by count (desc) then display path, for files/count output modes.
pub(super) fn collapse_to_files(matches: &[ContentSearchMatch]) -> Vec<FileMatchCount> {
    use std::collections::BTreeMap;
    let mut by_path: BTreeMap<&str, (&str, &str, usize)> = BTreeMap::new();
    for m in matches {
        let entry = by_path.entry(m.path.as_str()).or_insert((
            m.root_id.as_str(),
            m.display_path.as_str(),
            0,
        ));
        entry.2 += 1;
    }
    let mut files: Vec<FileMatchCount> = by_path
        .into_iter()
        .map(|(path, (root_id, display_path, count))| FileMatchCount {
            root_id: root_id.to_string(),
            path: path.to_string(),
            display_path: display_path.to_string(),
            count,
        })
        .collect();
    files.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(a.display_path.cmp(&b.display_path))
    });
    files
}
