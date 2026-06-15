//! Path and content search over immutable catalog snapshots.

use crate::{cancel::CancelToken, models::*, port::CatalogProvider, snapshot::CatalogSnapshot};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};

const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const PATH_SUBSTRING_BASE: i64 = 1_000_000;
const PATH_REGEX_BASE: i64 = 900_000;
const SCORE_SCALE: f64 = 1_000_000.0;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

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

    let mut path_matches = Vec::new();
    if matches!(request.mode, SearchMode::Path | SearchMode::Both) {
        let path_results: Vec<Result<Option<PathSearchMatch>, CtxError>> = snapshot
            .entries
            .par_iter()
            .map_init(
                || PathMatcherState::new(case_sensitive),
                |state, entry| {
                    cancel.check_cancelled()?;
                    Ok(path_score(&entry.rel_path, path_query, state).map(|score| {
                        PathSearchMatch {
                            root_id: entry.root_id.clone(),
                            path: entry.rel_path.clone(),
                            display_path: display_path(snapshot, &entry.root_id, &entry.rel_path),
                            score,
                        }
                    }))
                },
            )
            .collect();
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
            if content_entries.len() >= max_content_files
                || planned_bytes.saturating_add(entry.size) > max_content_bytes
            {
                content_exhausted = true;
                break;
            }
            planned_bytes = planned_bytes.saturating_add(entry.size);
            content_entries.push(entry);
        }

        let content_results: Vec<Result<ContentSearchResult, CtxError>> = content_entries
            .par_iter()
            .map(|entry| {
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
                        context_lines: request.context_lines,
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
            .collect();

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
        apply_content_relevance_scores(&mut content_matches, &ranking_stats, &ranking_query);
        content_matches.sort_by(content_match_cmp);
        if content_matches.len() > max_results {
            content_matches.truncate(max_results);
        }
    }

    let total = path_total + content_total;
    let returned = path_matches.len() + content_matches.len();
    let omitted = total.saturating_sub(returned);
    Ok(SearchResponse {
        path_matches,
        content_matches,
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

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
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
    context_lines: usize,
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
                context: line_context(&lines, idx, input.context_lines),
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

#[derive(Clone, Debug)]
enum ContentRankingQuery {
    /// Literal searches without `whole_word` use file-level BM25. A single term
    /// naturally degenerates to saturated term frequency plus BM25's document
    /// length normalization, so no cross-document IDF is needed to order files.
    Bm25 { terms: Vec<String> },
    /// Regex and `whole_word` searches do not expose stable lexical query terms:
    /// regex can match arbitrary syntax and whole-word filtering is applied to
    /// match spans. For those modes we fall back to TF density (matches per
    /// normalized document length) and document the intentional degradation here.
    TfDensity,
}

impl ContentRankingQuery {
    fn new(pattern: &str, case_sensitive: bool, regex: bool, whole_word: bool) -> Self {
        if regex || whole_word {
            return Self::TfDensity;
        }
        let terms = tokenize_query(pattern, case_sensitive);
        if terms.is_empty() {
            Self::TfDensity
        } else {
            Self::Bm25 { terms }
        }
    }

    fn stats_for_file(&self, path: &str, text: &str, occurrence_count: usize) -> FileRankingStats {
        match self {
            Self::Bm25 { terms } => collect_bm25_stats(path, text, terms),
            Self::TfDensity => FileRankingStats {
                path: path.to_string(),
                doc_len: normalized_doc_len(text),
                term_frequencies: HashMap::new(),
                occurrence_count,
            },
        }
    }
}

#[derive(Clone, Debug)]
struct FileRankingStats {
    path: String,
    doc_len: usize,
    term_frequencies: HashMap<String, usize>,
    occurrence_count: usize,
}

fn collect_bm25_stats(path: &str, text: &str, terms: &[String]) -> FileRankingStats {
    let query_terms: HashSet<&str> = terms.iter().map(String::as_str).collect();
    let case_sensitive = terms
        .iter()
        .any(|term| term.chars().any(char::is_uppercase));
    let mut doc_len = 0usize;
    let mut term_frequencies = HashMap::new();
    for token in tokenize_text(text, case_sensitive) {
        doc_len += 1;
        if query_terms.contains(token.as_str()) {
            *term_frequencies.entry(token).or_insert(0) += 1;
        }
    }
    FileRankingStats {
        path: path.to_string(),
        doc_len: doc_len.max(1),
        term_frequencies,
        occurrence_count: 0,
    }
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

fn content_file_scores(
    stats: &[FileRankingStats],
    query: &ContentRankingQuery,
) -> HashMap<String, i64> {
    match query {
        ContentRankingQuery::Bm25 { terms } => bm25_file_scores(stats, terms),
        ContentRankingQuery::TfDensity => tf_density_file_scores(stats),
    }
}

fn bm25_file_scores(stats: &[FileRankingStats], terms: &[String]) -> HashMap<String, i64> {
    if stats.is_empty() {
        return HashMap::new();
    }
    let unique_terms: Vec<&String> = terms.iter().collect();
    let doc_count = stats.len() as f64;
    let avg_doc_len = stats.iter().map(|stat| stat.doc_len as f64).sum::<f64>() / doc_count;
    let mut document_frequencies: HashMap<&str, usize> = HashMap::new();
    for term in &unique_terms {
        let df = stats
            .iter()
            .filter(|stat| {
                stat.term_frequencies
                    .get(term.as_str())
                    .copied()
                    .unwrap_or(0)
                    > 0
            })
            .count();
        document_frequencies.insert(term.as_str(), df);
    }

    stats
        .iter()
        .map(|stat| {
            let mut score = 0.0;
            for term in &unique_terms {
                let tf = stat
                    .term_frequencies
                    .get(term.as_str())
                    .copied()
                    .unwrap_or(0) as f64;
                if tf == 0.0 {
                    continue;
                }
                let length_norm = 1.0 - BM25_B + BM25_B * (stat.doc_len as f64 / avg_doc_len);
                let saturated_tf = (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * length_norm);
                if unique_terms.len() == 1 {
                    score += saturated_tf;
                } else {
                    let df = *document_frequencies.get(term.as_str()).unwrap_or(&0) as f64;
                    let idf = (1.0 + (doc_count - df + 0.5) / (df + 0.5)).ln();
                    score += idf * saturated_tf;
                }
            }
            (stat.path.clone(), scaled_score(score))
        })
        .collect()
}

fn tf_density_file_scores(stats: &[FileRankingStats]) -> HashMap<String, i64> {
    stats
        .iter()
        .map(|stat| {
            let density = stat.occurrence_count as f64 / stat.doc_len.max(1) as f64;
            (stat.path.clone(), scaled_score(density))
        })
        .collect()
}

fn scaled_score(score: f64) -> i64 {
    (score * SCORE_SCALE).round() as i64
}

fn tokenize_query(pattern: &str, case_sensitive: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for token in tokenize_text(pattern, case_sensitive) {
        if seen.insert(token.clone()) {
            terms.push(token);
        }
    }
    terms
}

fn tokenize_text(text: &str, case_sensitive: bool) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if case_sensitive {
                current.push(ch);
            } else {
                current.extend(ch.to_lowercase());
            }
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn normalized_doc_len(text: &str) -> usize {
    let token_len = tokenize_text(text, false).len();
    token_len.max(text.lines().count()).max(1)
}

fn line_context(lines: &[&str], match_idx: usize, context_lines: usize) -> Vec<LineContext> {
    let start = match_idx.saturating_sub(context_lines);
    let end = (match_idx + context_lines + 1).min(lines.len());
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
            context_lines: 2,
            pattern,
            regex: compiled_regex.as_ref(),
            ac: ac.as_ref(),
            whole_word,
        },
        &CancelToken::never(),
    )?;
    Ok(matches.occurrence_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, catalog::ScanOptions};
    use std::fs;

    fn request(pattern: &str, mode: SearchMode, max_results: usize) -> SearchRequest {
        SearchRequest {
            pattern: pattern.to_string(),
            mode,
            regex: false,
            max_results,
            context_lines: 0,
            max_content_files: 2_048,
            max_content_bytes: 64 * 1024 * 1024,
            whole_word: false,
        }
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
