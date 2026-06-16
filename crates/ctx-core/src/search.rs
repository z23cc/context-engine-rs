//! Path and content search over immutable catalog snapshots.

use crate::{
    cancel::CancelToken,
    models::*,
    port::CatalogProvider,
    ranking::{ContentRankingQuery, EntryFilter, FileRankingStats, content_file_scores, is_binary},
    snapshot::CatalogSnapshot,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

const PATH_SUBSTRING_BASE: i64 = 1_000_000;
const PATH_REGEX_BASE: i64 = 900_000;

/// Search an immutable snapshot using provider-backed content reads.
pub fn search_snapshot<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &SearchRequest,
) -> Result<SearchResponse, CtxError> {
    search_snapshot_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Search an immutable snapshot using provider-backed content reads, checking
/// `cancel` throughout the path/content hot loops.
pub fn search_snapshot_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &SearchRequest,
    cancel: &CancelToken,
) -> Result<SearchResponse, CtxError> {
    cancel.check_cancelled()?;
    let mut diagnostics = Vec::new();
    let mut path_total = 0usize;
    let max_results = request.max_results.max(1);
    let max_content_files = request.max_content_files;
    let max_content_bytes = request.max_content_bytes;
    let case_sensitive = is_smart_case_sensitive(&request.pattern);

    let regex = if request.regex {
        Some(
            RegexBuilder::new(&request.pattern)
                .case_insensitive(!case_sensitive)
                .build()?,
        )
    } else {
        None
    };
    let ac = if request.regex || request.pattern.is_empty() {
        None
    } else {
        Some(
            AhoCorasickBuilder::new()
                .ascii_case_insensitive(!case_sensitive)
                .build([request.pattern.as_str()])
                .expect("single literal pattern"),
        )
    };
    let fuzzy_pattern = (!request.regex).then(|| {
        Pattern::new(
            &request.pattern,
            nucleo_case_matching(case_sensitive),
            Normalization::Never,
            AtomKind::Fuzzy,
        )
    });
    let path_query = PathQuery {
        pattern: &request.pattern,
        regex: regex.as_ref(),
        case_sensitive,
        whole_word: request.whole_word,
        fuzzy_pattern: fuzzy_pattern.as_ref(),
    };
    let filter = EntryFilter::build(request)?;
    let context_before = request.context_before.unwrap_or(request.context_lines);
    let context_after = request.context_after.unwrap_or(request.context_lines);

    let mut path_matches = Vec::new();
    if matches!(request.mode, SearchMode::Path | SearchMode::Both) {
        let path_results: Vec<Result<Option<PathSearchMatch>, CtxError>> = {
            #[cfg(not(target_arch = "wasm32"))]
            {
                snapshot
                    .entries
                    .par_iter()
                    .map_init(
                        || PathMatcherState::new(case_sensitive),
                        |state, entry| {
                            cancel.check_cancelled()?;
                            if !filter.accepts(&entry.rel_path) {
                                return Ok(None);
                            }
                            Ok(path_score(&entry.rel_path, path_query, state).map(|score| {
                                PathSearchMatch {
                                    root_id: entry.root_id.clone(),
                                    path: entry.rel_path.clone(),
                                    display_path: display_path(
                                        snapshot,
                                        &entry.root_id,
                                        &entry.rel_path,
                                    ),
                                    score,
                                }
                            }))
                        },
                    )
                    .collect()
            }
            #[cfg(target_arch = "wasm32")]
            {
                let mut state = PathMatcherState::new(case_sensitive);
                snapshot
                    .entries
                    .iter()
                    .map(|entry| {
                        cancel.check_cancelled()?;
                        if !filter.accepts(&entry.rel_path) {
                            return Ok(None);
                        }
                        Ok(
                            path_score(&entry.rel_path, path_query, &mut state).map(|score| {
                                PathSearchMatch {
                                    root_id: entry.root_id.clone(),
                                    path: entry.rel_path.clone(),
                                    display_path: display_path(
                                        snapshot,
                                        &entry.root_id,
                                        &entry.rel_path,
                                    ),
                                    score,
                                }
                            }),
                        )
                    })
                    .collect()
            }
        };
        let mut path_hits = Vec::new();
        for result in path_results {
            if let Some(hit) = result? {
                path_hits.push(hit);
            }
        }
        path_total = path_hits.len();
        path_hits.sort_by(path_match_cmp);
        if path_hits.len() > max_results {
            path_hits.truncate(max_results);
        }
        path_matches = path_hits;
    }

    let mut content_total = 0usize;
    let content_files_scanned = AtomicUsize::new(0);
    let content_bytes_scanned = AtomicU64::new(0);
    let binary_files_skipped = AtomicUsize::new(0);
    let mut content_exhausted = false;
    let mut content_matches = Vec::new();
    let mut match_files: Vec<FileMatchCount> = Vec::new();
    let mut ranking_stats = Vec::new();

    if matches!(request.mode, SearchMode::Content | SearchMode::Both) && !request.pattern.is_empty()
    {
        let ranking_query = ContentRankingQuery::new(
            &request.pattern,
            case_sensitive,
            request.regex,
            request.whole_word,
        );
        let mut planned_bytes = 0u64;
        let mut content_entries = Vec::new();
        for entry in &snapshot.entries {
            cancel.check_cancelled()?;
            if !filter.accepts(&entry.rel_path) {
                continue;
            }
            if content_entries.len() >= max_content_files
                || planned_bytes.saturating_add(entry.size) > max_content_bytes
            {
                content_exhausted = true;
                break;
            }
            planned_bytes = planned_bytes.saturating_add(entry.size);
            content_entries.push(entry);
        }

        let content_results: Vec<Result<ContentSearchResult, CtxError>> = {
            #[cfg(not(target_arch = "wasm32"))]
            let iter = content_entries.par_iter();
            #[cfg(target_arch = "wasm32")]
            let iter = content_entries.iter();

            iter.map(|entry| {
                cancel.check_cancelled()?;
                let bytes = provider.read_bytes(Path::new(&entry.abs_path))?;
                cancel.check_cancelled()?;
                let actual_len = bytes.len() as u64;
                content_files_scanned.fetch_add(1, Ordering::Relaxed);
                // The deterministic budget plan uses cataloged snapshot sizes so
                // file choice is stable. The parallel byte counter records actual
                // bytes read; if files grow after cataloging, this can become a
                // close upper-bound approximation over max_content_bytes.
                content_bytes_scanned.fetch_add(actual_len, Ordering::Relaxed);

                if is_binary(&bytes) {
                    binary_files_skipped.fetch_add(1, Ordering::Relaxed);
                    return Ok(ContentSearchResult {
                        matches: Vec::new(),
                        ranking: None,
                        diagnostic: Some(Diagnostic {
                            path: Some(PathBuf::from(&entry.rel_path)),
                            message: format!(
                                "skipped binary file during content search: {}",
                                entry.rel_path
                            ),
                        }),
                    });
                }

                let text = String::from_utf8_lossy(&bytes);
                let display_path = display_path(snapshot, &entry.root_id, &entry.rel_path);
                let match_set = content_matches_for_file(
                    ContentMatchInput {
                        text: &text,
                        root_id: &entry.root_id,
                        path: &entry.rel_path,
                        display_path: &display_path,
                        context_before,
                        context_after,
                        pattern: &request.pattern,
                        regex: regex.as_ref(),
                        ac: ac.as_ref(),
                        whole_word: request.whole_word,
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
            })
            .collect()
        };

        for result in content_results {
            let found = result?;
            content_total += found.matches.len();
            content_matches.extend(found.matches);
            if let Some(stats) = found.ranking {
                ranking_stats.push(stats);
            }
            if let Some(diagnostic) = found.diagnostic {
                diagnostics.push(diagnostic);
            }
        }

        cancel.check_cancelled()?;
        match request.output_mode {
            OutputMode::Content => {
                apply_content_relevance_scores(
                    &mut content_matches,
                    &ranking_stats,
                    &ranking_query,
                );
                content_matches.sort_by(content_match_cmp);
                if content_matches.len() > max_results {
                    content_matches.truncate(max_results);
                }
            }
            OutputMode::FilesWithMatches | OutputMode::Count => {
                match_files = collapse_to_files(&content_matches);
                if match_files.len() > max_results {
                    match_files.truncate(max_results);
                }
                content_matches = Vec::new();
            }
        }
    }

    let total = path_total + content_total;
    let returned = path_matches.len() + content_matches.len();
    let omitted = total.saturating_sub(returned);
    Ok(SearchResponse {
        path_matches,
        content_matches,
        match_files,
        diagnostics,
        totals: SearchTotals {
            scanned_files: snapshot.entries.len(),
            path_matches: path_total,
            content_matches: content_total,
            omitted,
            content_files_scanned: content_files_scanned.load(Ordering::Relaxed),
            content_bytes_scanned: content_bytes_scanned.load(Ordering::Relaxed),
            binary_files_skipped: binary_files_skipped.load(Ordering::Relaxed),
            content_file_limit: max_content_files,
            content_byte_limit: max_content_bytes,
            totals_are_lower_bound: content_exhausted,
            budget: SearchBudget {
                max_results,
                max_content_files,
                max_content_bytes,
                exhausted: omitted > 0 || content_exhausted,
            },
        },
    })
}

fn path_match_cmp(left: &PathSearchMatch, right: &PathSearchMatch) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.cmp(&right.path))
}

fn content_match_cmp(left: &ContentSearchMatch, right: &ContentSearchMatch) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.cmp(&right.path))
        .then_with(|| left.line.cmp(&right.line))
}

fn display_path(snapshot: &CatalogSnapshot, root_id: &str, rel_path: &str) -> String {
    let Some(root) = snapshot.roots.iter().find(|root| root.id == root_id) else {
        return rel_path.to_string();
    };
    let root_name = root.path.file_name().unwrap_or_default().to_string_lossy();
    if rel_path.is_empty() {
        root_name.into_owned()
    } else {
        format!("{root_name}/{rel_path}")
    }
}

#[derive(Clone, Copy)]
struct PathQuery<'a> {
    pattern: &'a str,
    regex: Option<&'a Regex>,
    case_sensitive: bool,
    whole_word: bool,
    fuzzy_pattern: Option<&'a Pattern>,
}

struct PathMatcherState {
    matcher: Matcher,
    utf32_buf: Vec<char>,
    indices: Vec<u32>,
}

impl PathMatcherState {
    fn new(case_sensitive: bool) -> Self {
        let mut config = Config::DEFAULT.match_paths();
        config.ignore_case = !case_sensitive;
        config.normalize = false;
        config.prefer_prefix = true;
        Self {
            matcher: Matcher::new(config),
            utf32_buf: Vec::new(),
            indices: Vec::new(),
        }
    }
}

fn path_score(path: &str, query: PathQuery<'_>, state: &mut PathMatcherState) -> Option<i64> {
    if query.pattern.is_empty() {
        return Some(0);
    }
    if let Some(regex) = query.regex {
        return first_regex_match(path, regex, query.whole_word)
            .map(|(start, _end)| PATH_REGEX_BASE - path.len() as i64 - (start as i64 * 2));
    }

    if let Some((start, end)) =
        first_literal_match(path, query.pattern, query.case_sensitive, query.whole_word)
    {
        let nucleo_score = nucleo_fuzzy_score(path, query, state).unwrap_or_default();
        return Some(
            PATH_SUBSTRING_BASE + nucleo_score - path.len() as i64 - (start as i64 * 2)
                + if end == path.len() { 4 } else { 0 },
        );
    }

    nucleo_fuzzy_score(path, query, state)
}

fn nucleo_fuzzy_score(
    path: &str,
    query: PathQuery<'_>,
    state: &mut PathMatcherState,
) -> Option<i64> {
    let pattern = query.fuzzy_pattern?;
    state.utf32_buf.clear();
    let haystack = Utf32Str::new(path, &mut state.utf32_buf);
    if query.whole_word {
        state.indices.clear();
        let score = pattern.indices(haystack, &mut state.matcher, &mut state.indices)?;
        fuzzy_indices_respect_whole_word(path, &state.indices).then_some(score as i64)
    } else {
        pattern
            .score(haystack, &mut state.matcher)
            .map(|score| score as i64)
    }
}

fn fuzzy_indices_respect_whole_word(path: &str, indices: &[u32]) -> bool {
    let Some(first) = indices.iter().min().copied() else {
        return false;
    };
    let Some(last) = indices.iter().max().copied() else {
        return false;
    };
    let chars: Vec<char> = path.chars().collect();
    let first = first as usize;
    let last = last as usize;
    (first == 0 || !is_word_char(chars[first - 1]))
        && (last + 1 >= chars.len() || !is_word_char(chars[last + 1]))
}

fn nucleo_case_matching(case_sensitive: bool) -> CaseMatching {
    if case_sensitive {
        CaseMatching::Respect
    } else {
        CaseMatching::Ignore
    }
}

fn first_regex_match(text: &str, regex: &Regex, whole_word: bool) -> Option<(usize, usize)> {
    regex.find_iter(text).find_map(|mat| {
        let span = (mat.start(), mat.end());
        (!whole_word || is_whole_word_match(text, span.0, span.1)).then_some(span)
    })
}

fn first_literal_match(
    text: &str,
    pattern: &str,
    case_sensitive: bool,
    whole_word: bool,
) -> Option<(usize, usize)> {
    if pattern.is_empty() {
        return Some((0, 0));
    }

    let mut offset = 0usize;
    while offset <= text.len().saturating_sub(pattern.len()) {
        let found = if case_sensitive {
            text[offset..].find(pattern)
        } else {
            find_ascii_case_insensitive(&text.as_bytes()[offset..], pattern.as_bytes())
        };
        let start = offset + found?;
        let end = start + pattern.len();
        if !whole_word || is_whole_word_match(text, start, end) {
            return Some((start, end));
        }
        offset = next_char_boundary(text, start);
    }
    None
}

fn find_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle))
}

fn next_char_boundary(text: &str, byte_idx: usize) -> usize {
    text[byte_idx..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(offset, _)| byte_idx + offset)
}

fn is_smart_case_sensitive(pattern: &str) -> bool {
    pattern.chars().any(char::is_uppercase)
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_whole_word_match(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|ch| !is_word_char(ch)) && after.is_none_or(|ch| !is_word_char(ch))
}

struct ContentSearchResult {
    matches: Vec<ContentSearchMatch>,
    ranking: Option<FileRankingStats>,
    diagnostic: Option<Diagnostic>,
}

struct ContentMatchInput<'a> {
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

struct ContentMatchSet {
    matches: Vec<ContentSearchMatch>,
    occurrence_count: usize,
}

fn content_matches_for_file(
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

fn find_content_match_columns(
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

fn literal_match_columns(
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

fn apply_content_relevance_scores(
    matches: &mut [ContentSearchMatch],
    stats: &[FileRankingStats],
    query: &ContentRankingQuery,
) {
    let scores = content_file_scores(stats, query);
    for hit in matches {
        hit.score = scores.get(&hit.path).copied().unwrap_or_default();
    }
}

fn line_context(
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

fn trim_preview(line: &str) -> String {
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
fn collapse_to_files(matches: &[ContentSearchMatch]) -> Vec<FileMatchCount> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranking::glob_to_regex;
    use crate::{FsCatalogProvider, RootPolicy, catalog::ScanOptions};
    use std::fs;

    fn request(pattern: &str, mode: SearchMode, max_results: usize) -> SearchRequest {
        SearchRequest {
            pattern: pattern.to_string(),
            mode,
            max_results,
            context_lines: 0,
            ..SearchRequest::default()
        }
    }

    fn filtered(pattern: &str, f: impl FnOnce(&mut SearchRequest)) -> SearchRequest {
        let mut req = SearchRequest {
            pattern: pattern.to_string(),
            mode: SearchMode::Content,
            ..SearchRequest::default()
        };
        f(&mut req);
        req
    }

    #[test]
    fn glob_to_regex_segment_and_recursive_semantics() {
        // bare glob with no slash matches basename at any depth
        let any = Regex::new(&glob_to_regex("*.rs")).unwrap();
        assert!(any.is_match("a.rs"));
        assert!(any.is_match("src/deep/b.rs"));
        assert!(!any.is_match("a.txt"));
        // anchored dir glob: * stays within a segment
        let scoped = Regex::new(&glob_to_regex("src/*.rs")).unwrap();
        assert!(scoped.is_match("src/a.rs"));
        assert!(!scoped.is_match("src/deep/a.rs"));
        // ** crosses segments
        let deep = Regex::new(&glob_to_regex("src/**/*.rs")).unwrap();
        assert!(deep.is_match("src/deep/a.rs"));
        assert!(deep.is_match("src/a.rs"));
    }

    #[test]
    fn extension_and_glob_filters_narrow_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "needle\n").expect("w");
        fs::write(dir.path().join("b.txt"), "needle\n").expect("w");
        std::fs::create_dir(dir.path().join("vendor")).expect("mkdir");
        fs::write(dir.path().join("vendor/c.rs"), "needle\n").expect("w");
        let (provider, snapshot) = provider_for(dir.path());

        let by_ext = search_snapshot(
            &provider,
            &snapshot,
            &filtered("needle", |r| r.extensions = vec!["rs".into()]),
        )
        .expect("ext");
        assert!(
            by_ext
                .content_matches
                .iter()
                .all(|m| m.path.ends_with(".rs"))
        );
        assert_eq!(by_ext.content_matches.len(), 2);

        let excluded = search_snapshot(
            &provider,
            &snapshot,
            &filtered("needle", |r| {
                r.extensions = vec!["rs".into()];
                r.exclude = vec!["vendor/**".into()];
            }),
        )
        .expect("exclude");
        assert_eq!(excluded.content_matches.len(), 1);
        assert_eq!(excluded.content_matches[0].path, "a.rs");
    }

    #[test]
    fn output_mode_files_and_count_collapse_to_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "needle\nneedle\n").expect("w");
        fs::write(dir.path().join("b.rs"), "needle\n").expect("w");
        let (provider, snapshot) = provider_for(dir.path());

        let files = search_snapshot(
            &provider,
            &snapshot,
            &filtered("needle", |r| r.output_mode = OutputMode::FilesWithMatches),
        )
        .expect("files");
        assert!(files.content_matches.is_empty());
        assert_eq!(files.match_files.len(), 2);
        // ordered by count desc: a.rs (2) before b.rs (1)
        assert_eq!(files.match_files[0].path, "a.rs");
        assert_eq!(files.match_files[0].count, 2);
    }

    #[test]
    fn asymmetric_context_before_after() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "l1\nl2\nMATCH\nl4\nl5\n").expect("w");
        let (provider, snapshot) = provider_for(dir.path());
        let resp = search_snapshot(
            &provider,
            &snapshot,
            &filtered("MATCH", |r| {
                r.context_before = Some(2);
                r.context_after = Some(0);
            }),
        )
        .expect("ctx");
        let ctx = &resp.content_matches[0].context;
        // lines 1,2,3 (two before + the match), none after
        assert_eq!(ctx.first().unwrap().line, 1);
        assert_eq!(ctx.last().unwrap().line, 3);
    }

    fn provider_for(dir: &Path) -> (FsCatalogProvider, CatalogSnapshot) {
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        (provider, snapshot)
    }

    fn score_path(
        path: &str,
        pattern: &str,
        case_sensitive: bool,
        whole_word: bool,
    ) -> Option<i64> {
        let fuzzy_pattern = Pattern::new(
            pattern,
            nucleo_case_matching(case_sensitive),
            Normalization::Never,
            AtomKind::Fuzzy,
        );
        let query = PathQuery {
            pattern,
            regex: None,
            case_sensitive,
            whole_word,
            fuzzy_pattern: Some(&fuzzy_pattern),
        };
        let mut state = PathMatcherState::new(case_sensitive);
        path_score(path, query, &mut state)
    }

    #[test]
    fn finds_path_and_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("src")).expect("src");
        fs::write(dir.path().join("src/lib.rs"), "pub fn needle() {}\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Both, 10),
        )
        .expect("search");
        assert_eq!(response.totals.content_matches, 1);
        assert_eq!(response.content_matches.len(), 1);
    }

    #[test]
    fn returns_per_bucket_top_k_after_independent_ranking() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("zzz_needles.txt"), "needle late path\n").expect("write zzz");
        fs::write(dir.path().join("needle.rs"), "pub fn unrelated() {}\n").expect("write needle");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Both, 1),
        )
        .expect("search");
        assert_eq!(
            response.totals.path_matches + response.totals.content_matches,
            3
        );
        assert_eq!(response.totals.omitted, 1);
        assert_eq!(response.path_matches.len(), 1);
        assert_eq!(response.path_matches[0].path, "needle.rs");
        assert_eq!(response.content_matches.len(), 1);
        assert_eq!(response.content_matches[0].path, "zzz_needles.txt");
    }

    #[test]
    fn nucleo_path_scoring_prefers_boundaries_and_contiguous_runs() {
        let boundary = score_path("src/FooBar.rs", "fb", false, false).expect("boundary");
        let word_middle = score_path("src/afobb.rs", "fb", false, false).expect("middle");
        assert!(boundary > word_middle);

        let contiguous = score_path("src/foo_bar.rs", "foo", false, false).expect("contiguous");
        let jumping = score_path("src/f_a_o_o.rs", "foo", false, false).expect("jumping");
        assert!(contiguous > jumping);

        let substring = score_path("src/foo.rs", "foo", false, false).expect("substring");
        assert!(substring > contiguous);
    }

    #[test]
    fn path_results_are_sorted_by_nucleo_score() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("src");
        fs::write(dir.path().join("src/FooBar.rs"), "").expect("boundary");
        fs::write(dir.path().join("src/afobb.rs"), "").expect("middle");
        fs::write(dir.path().join("src/f_a_o_o.rs"), "").expect("jumping");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(&provider, &snapshot, &request("fb", SearchMode::Path, 10))
            .expect("search");
        assert_eq!(response.path_matches[0].path, "src/FooBar.rs");
    }

    #[test]
    fn bm25_multi_term_content_ranking_prefers_relevant_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("focused.txt"),
            "alpha beta alpha beta alpha beta\n",
        )
        .expect("focused");
        fs::write(
            dir.path().join("diluted.txt"),
            format!("alpha beta {}\n", "filler ".repeat(200)),
        )
        .expect("diluted");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("alpha beta", SearchMode::Content, 10),
        )
        .expect("search");
        assert_eq!(response.content_matches[0].path, "focused.txt");
        assert!(response.content_matches[0].score > response.content_matches[1].score);
    }

    #[test]
    fn single_term_content_ranking_uses_tf_saturation_and_length_norm() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("dense.txt"), "needle needle needle\n").expect("dense");
        fs::write(
            dir.path().join("sparse.txt"),
            format!("needle {}\n", "filler ".repeat(200)),
        )
        .expect("sparse");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
        )
        .expect("search");
        assert_eq!(response.content_matches[0].path, "dense.txt");
        assert!(response.content_matches[0].score > response.content_matches[1].score);
    }

    #[test]
    fn regex_content_ranking_falls_back_to_tf_density() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("dense.txt"), "needle\nneedle\nneedle\n").expect("dense");
        fs::write(
            dir.path().join("sparse.txt"),
            format!("needle\n{}\n", "filler\n".repeat(200)),
        )
        .expect("sparse");
        let (provider, snapshot) = provider_for(dir.path());
        let mut req = request(r"need(le)?", SearchMode::Content, 10);
        req.regex = true;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.content_matches[0].path, "dense.txt");
        assert!(response.content_matches[0].score > response.content_matches.last().unwrap().score);
    }

    #[test]
    fn smart_case_literal_search_is_insensitive_until_pattern_has_uppercase() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("case.txt"), "Needle\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());

        let insensitive = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
        )
        .expect("insensitive search");
        assert_eq!(insensitive.totals.content_matches, 1);

        let sensitive = search_snapshot(
            &provider,
            &snapshot,
            &request("NeedleX", SearchMode::Content, 10),
        )
        .expect("sensitive search");
        assert_eq!(sensitive.totals.content_matches, 0);
    }

    #[test]
    fn smart_case_regex_search_is_insensitive_until_pattern_has_uppercase() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("case.txt"), "Needle\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());

        let mut insensitive = request("needle", SearchMode::Content, 10);
        insensitive.regex = true;
        let insensitive =
            search_snapshot(&provider, &snapshot, &insensitive).expect("regex search");
        assert_eq!(insensitive.totals.content_matches, 1);

        let mut sensitive = request("needle[A-Z]", SearchMode::Content, 10);
        sensitive.regex = true;
        let sensitive = search_snapshot(&provider, &snapshot, &sensitive).expect("regex search");
        assert_eq!(sensitive.totals.content_matches, 0);
    }

    #[test]
    fn binary_files_are_skipped_for_content_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("binary.bin"), b"needle\0needle\n").expect("write binary");
        fs::write(dir.path().join("text.txt"), "needle\n").expect("write text");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
        )
        .expect("search");

        assert_eq!(response.totals.content_files_scanned, 2);
        assert_eq!(response.totals.binary_files_skipped, 1);
        assert_eq!(response.totals.content_matches, 1);
        assert_eq!(response.content_matches[0].path, "text.txt");
        assert_eq!(response.diagnostics.len(), 1);
        assert_eq!(
            response.diagnostics[0].path,
            Some(PathBuf::from("binary.bin"))
        );
    }

    #[test]
    fn whole_word_literal_filters_subword_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("words.txt"), "needle needless\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let mut req = request("needle", SearchMode::Content, 10);
        req.whole_word = true;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.totals.content_matches, 1);
        assert_eq!(response.content_matches[0].column, 1);
    }

    #[test]
    fn enforces_content_file_limit_with_lower_bound_totals() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "needle one\n").expect("write a");
        fs::write(dir.path().join("b.txt"), "needle two\n").expect("write b");
        let (provider, snapshot) = provider_for(dir.path());
        let mut req = request("needle", SearchMode::Content, 10);
        req.max_content_files = 1;
        req.max_content_bytes = 64 * 1024;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.totals.content_files_scanned, 1);
        assert_eq!(response.content_matches.len(), 1);
        assert!(response.totals.totals_are_lower_bound);
        assert!(response.totals.budget.exhausted);
    }

    #[test]
    fn enforces_content_byte_limit_before_reading_next_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "needle one\n").expect("write a");
        fs::write(dir.path().join("b.txt"), "needle two\n").expect("write b");
        let (provider, snapshot) = provider_for(dir.path());
        let first_size = snapshot
            .entries
            .iter()
            .find(|entry| entry.rel_path == "a.txt")
            .expect("a entry")
            .size;
        let mut req = request("needle", SearchMode::Content, 10);
        req.max_content_files = 10;
        req.max_content_bytes = first_size;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.totals.content_files_scanned, 1);
        assert_eq!(response.totals.content_bytes_scanned, first_size);
        assert!(response.totals.totals_are_lower_bound);
        assert!(response.totals.budget.exhausted);
    }

    #[test]
    fn pre_cancelled_content_search_returns_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("text.txt"), "needle\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let token = CancelToken::new();
        token.cancel();

        let err = search_snapshot_cancellable(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
            &token,
        )
        .expect_err("search should cancel");
        assert!(matches!(err, CtxError::Cancelled));
    }

    #[test]
    fn content_search_cancel_after_n_checks_is_deterministic() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("text.txt"),
            "needle\nneedle\nneedle\nneedle\n",
        )
        .expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let token = CancelToken::cancel_after_checks(5);

        let err = search_snapshot_cancellable(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
            &token,
        )
        .expect_err("content search should cancel after injected check count");
        assert!(matches!(err, CtxError::Cancelled));
    }
}
