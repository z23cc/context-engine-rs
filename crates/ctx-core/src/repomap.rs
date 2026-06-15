//! PageRank-based repository map over the lightweight codemap.
//!
//! This module intentionally keeps the implementation pure Rust. It builds a
//! symbol-definition index from the existing top-level codemap, tokenizes source
//! files with an identifier-occurrence heuristic, then runs deterministic sparse
//! personalized PageRank over the resulting file graph.
//!
//! Important limitation: reference edges are **not** scope/import aware. A token
//! named like a symbol defined in another file is treated as a reference to that
//! symbol's defining file(s). Aider's production repo-map uses tree-sitter
//! reference tags; this Rust slice approximates that behavior with identifier
//! occurrences so it remains dependency-light and portable.

use crate::{
    cancel::CancelToken,
    codemap::CodeSymbol,
    models::{CatalogEntry, CtxError, Diagnostic},
    port::CatalogProvider,
    snapshot::CatalogSnapshot,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
};

const DAMPING: f64 = 0.85;
const ITERATIONS: usize = 30;
const DEFAULT_MAX_FILES: usize = 20;
const MAX_SYMBOLS_PER_FILE: usize = 12;

/// Request for `get_repo_map`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapRequest {
    /// Optional literal query. Matching indexed files become personalized
    /// PageRank seeds. Smart-case matching is used for both path and content.
    pub query: Option<String>,
    /// Optional explicit file or directory seeds, relative to an allowed root or
    /// absolute paths inside an allowed root.
    #[serde(default)]
    pub seed_paths: Vec<PathBuf>,
    /// Maximum number of ranked files to return.
    pub max_files: usize,
}

impl Default for RepoMapRequest {
    fn default() -> Self {
        Self {
            query: None,
            seed_paths: Vec::new(),
            max_files: DEFAULT_MAX_FILES,
        }
    }
}

/// One ranked repository-map file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapFile {
    pub rank: usize,
    pub path: String,
    pub display_path: String,
    pub language: String,
    /// Fixed-precision PageRank score string for portable goldens.
    pub score: String,
    pub symbols: Vec<CodeSymbol>,
}

/// Telemetry for `get_repo_map`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapTotals {
    pub scanned_files: usize,
    pub indexed_files: usize,
    pub symbols_indexed: usize,
    pub edges: usize,
    pub seed_files: usize,
    pub omitted_files: usize,
    pub max_files: usize,
    pub damping: String,
    pub iterations: usize,
}

/// Response for `get_repo_map`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMapResponse {
    pub files: Vec<RepoMapFile>,
    pub diagnostics: Vec<Diagnostic>,
    pub totals: RepoMapTotals,
    /// Documents the approximation used to build reference edges.
    pub reference_heuristic: String,
}

/// Build a PageRank repo-map from the catalog snapshot.
pub fn get_repo_map<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &RepoMapRequest,
) -> Result<RepoMapResponse, CtxError> {
    get_repo_map_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Build a PageRank repo-map from the catalog snapshot, checking `cancel` in
/// file analysis, graph construction, and each PageRank iteration.
pub fn get_repo_map_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &RepoMapRequest,
    cancel: &CancelToken,
) -> Result<RepoMapResponse, CtxError> {
    cancel.check_cancelled()?;
    let query = normalized_query(request.query.as_deref());
    let seed_paths = normalize_seed_paths(&request.seed_paths);
    let max_files = request.max_files.max(1);

    let analyses = analyze_files_cancellable(provider, snapshot, query.as_deref(), cancel)?;
    let mut diagnostics = Vec::new();
    let mut files = Vec::new();
    let mut omitted_files = 0usize;

    for analysis in analyses {
        match analysis {
            FileAnalysisResult::Indexed(file) => files.push(file),
            FileAnalysisResult::Unsupported => omitted_files += 1,
            FileAnalysisResult::Diagnostic(diagnostic) => diagnostics.push(diagnostic),
        }
    }

    files.sort_by(|left, right| left.path.cmp(&right.path));

    let graph = ReferenceGraph::build(&files);
    let seed_indices = seed_indices(&files, &seed_paths);
    let seed_count = seed_indices.len();
    let personalization = personalization(files.len(), &seed_indices);
    let scores = page_rank_cancellable(&graph.edges, &personalization, cancel)?;

    let mut ranked: Vec<(usize, f64)> = scores.into_iter().enumerate().collect();
    ranked.sort_by(|(left_idx, left_score), (right_idx, right_score)| {
        score_cmp(*left_score, *right_score)
            .then_with(|| files[*left_idx].path.cmp(&files[*right_idx].path))
    });

    let total_ranked = ranked.len();
    if ranked.len() > max_files {
        ranked.truncate(max_files);
    }

    let response_files = ranked
        .into_iter()
        .enumerate()
        .map(|(position, (idx, score))| RepoMapFile {
            rank: position + 1,
            path: files[idx].path.clone(),
            display_path: files[idx].display_path.clone(),
            language: files[idx].language.clone(),
            score: format!("{score:.8}"),
            symbols: key_symbols(&files[idx].symbols),
        })
        .collect();

    Ok(RepoMapResponse {
        files: response_files,
        diagnostics,
        totals: RepoMapTotals {
            scanned_files: snapshot.entries.len(),
            indexed_files: files.len(),
            symbols_indexed: graph.symbols_indexed,
            edges: graph.edge_count,
            seed_files: seed_count,
            omitted_files: omitted_files + total_ranked.saturating_sub(max_files),
            max_files,
            damping: format!("{DAMPING:.2}"),
            iterations: ITERATIONS,
        },
        reference_heuristic:
            "language-scoped identifier heuristic; skips comments/strings and stopwords; not scope/import aware"
                .to_string(),
    })
}

#[cfg(test)]
fn analyze_files<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    query: Option<&str>,
) -> Result<Vec<FileAnalysisResult>, CtxError> {
    analyze_files_cancellable(provider, snapshot, query, &CancelToken::never())
}

fn analyze_files_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    query: Option<&str>,
    cancel: &CancelToken,
) -> Result<Vec<FileAnalysisResult>, CtxError> {
    snapshot
        .entries
        .par_iter()
        .map(|entry| analyze_file(provider, snapshot, entry, query, cancel))
        .collect()
}

fn analyze_file<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    entry: &CatalogEntry,
    query: Option<&str>,
    cancel: &CancelToken,
) -> Result<FileAnalysisResult, CtxError> {
    cancel.check_cancelled()?;
    let bytes = provider.read_bytes(&entry.abs_path)?;
    cancel.check_cancelled()?;
    let source = String::from_utf8_lossy(&bytes);
    let Some((language, symbols)) =
        (match provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? {
            Ok(result) => result,
            Err(message) => {
                return Ok(FileAnalysisResult::Diagnostic(Diagnostic {
                    path: Some(PathBuf::from(&entry.rel_path)),
                    message,
                }));
            }
        })
    else {
        return Ok(FileAnalysisResult::Unsupported);
    };

    let identifiers = identifier_counts_cancellable(&source, &language, cancel)?;

    Ok(FileAnalysisResult::Indexed(IndexedFile {
        path: entry.rel_path.clone(),
        display_path: display_path(snapshot, &entry.root_id, &entry.rel_path),
        abs_path: entry.abs_path.clone(),
        language,
        symbols: symbols.as_ref().clone(),
        identifiers,
        query_match: query.is_some_and(|needle| query_matches(&entry.rel_path, &source, needle)),
    }))
}

#[derive(Debug)]
enum FileAnalysisResult {
    Indexed(IndexedFile),
    Unsupported,
    Diagnostic(Diagnostic),
}

#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    display_path: String,
    abs_path: PathBuf,
    language: String,
    symbols: Vec<CodeSymbol>,
    identifiers: HashMap<String, usize>,
    query_match: bool,
}

#[derive(Debug)]
struct ReferenceGraph {
    edges: Vec<Vec<(usize, f64)>>,
    symbols_indexed: usize,
    edge_count: usize,
}

impl ReferenceGraph {
    fn build(files: &[IndexedFile]) -> Self {
        Self::build_cancellable(files, &CancelToken::never()).expect("never-cancel token")
    }

    fn build_cancellable(files: &[IndexedFile], cancel: &CancelToken) -> Result<Self, CtxError> {
        let language_file_counts = language_file_counts(files);
        let definitions = definition_index(files, &language_file_counts);
        let mut edge_maps = vec![BTreeMap::<usize, f64>::new(); files.len()];

        for (referencer_idx, file) in files.iter().enumerate() {
            cancel.check_cancelled()?;
            let mut identifiers: Vec<_> = file.identifiers.iter().collect();
            identifiers.sort_by_key(|(identifier, _)| *identifier);
            for (identifier, count) in identifiers {
                let Some(definers) = definitions
                    .get(&file.language)
                    .and_then(|by_name| by_name.get(identifier.as_str()))
                else {
                    continue;
                };
                for definer_idx in definers {
                    if *definer_idx == referencer_idx {
                        continue;
                    }
                    *edge_maps[referencer_idx].entry(*definer_idx).or_insert(0.0) += *count as f64;
                }
            }
        }

        let edge_count = edge_maps.iter().map(BTreeMap::len).sum();
        let edges = edge_maps
            .into_iter()
            .map(|map| map.into_iter().collect())
            .collect();

        Ok(Self {
            edges,
            symbols_indexed: definitions
                .values()
                .flat_map(BTreeMap::values)
                .map(BTreeSet::len)
                .sum(),
            edge_count,
        })
    }
}

fn language_file_counts(files: &[IndexedFile]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for file in files {
        *counts.entry(file.language.clone()).or_insert(0) += 1;
    }
    counts
}

fn definition_index(
    files: &[IndexedFile],
    language_file_counts: &BTreeMap<String, usize>,
) -> BTreeMap<String, BTreeMap<String, BTreeSet<usize>>> {
    let mut definitions: BTreeMap<String, BTreeMap<String, BTreeSet<usize>>> = BTreeMap::new();
    for (idx, file) in files.iter().enumerate() {
        for symbol in &file.symbols {
            if !is_reference_stopword(&symbol.name, &file.language) {
                definitions
                    .entry(file.language.clone())
                    .or_default()
                    .entry(symbol.name.clone())
                    .or_default()
                    .insert(idx);
            }
        }
    }

    for (language, by_name) in &mut definitions {
        let file_count = language_file_counts
            .get(language)
            .copied()
            .unwrap_or_default();
        by_name.retain(|_, definers| !is_high_document_frequency(definers.len(), file_count));
    }

    definitions
}

fn page_rank_cancellable(
    edges: &[Vec<(usize, f64)>],
    personalization: &[f64],
    cancel: &CancelToken,
) -> Result<Vec<f64>, CtxError> {
    let n = edges.len();
    if n == 0 {
        return Ok(Vec::new());
    }

    let mut ranks = personalization.to_vec();
    let mut next = vec![0.0; n];

    for _ in 0..ITERATIONS {
        cancel.check_cancelled()?;
        next.fill(0.0);
        let mut dangling = 0.0;

        for (source_idx, outgoing) in edges.iter().enumerate() {
            let rank = ranks[source_idx];
            if outgoing.is_empty() {
                dangling += rank;
                continue;
            }

            let total_weight: f64 = outgoing.iter().map(|(_, weight)| *weight).sum();
            if total_weight == 0.0 {
                dangling += rank;
                continue;
            }

            for (target_idx, weight) in outgoing {
                next[*target_idx] += rank * (*weight / total_weight);
            }
        }

        for idx in 0..n {
            next[idx] = (1.0 - DAMPING) * personalization[idx]
                + DAMPING * (next[idx] + dangling * personalization[idx]);
        }

        std::mem::swap(&mut ranks, &mut next);
    }

    Ok(ranks)
}

fn personalization(n: usize, seed_indices: &BTreeSet<usize>) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    if seed_indices.is_empty() {
        return vec![1.0 / n as f64; n];
    }

    let seed_weight = 1.0 / seed_indices.len() as f64;
    let mut vector = vec![0.0; n];
    for idx in seed_indices {
        vector[*idx] = seed_weight;
    }
    vector
}

fn seed_indices(files: &[IndexedFile], seed_paths: &[String]) -> BTreeSet<usize> {
    let mut seeds = BTreeSet::new();
    for (idx, file) in files.iter().enumerate() {
        if file.query_match || seed_paths.iter().any(|seed| path_matches_seed(file, seed)) {
            seeds.insert(idx);
        }
    }
    seeds
}

fn path_matches_seed(file: &IndexedFile, seed: &str) -> bool {
    if seed.is_empty() {
        return false;
    }
    file.path == seed
        || file.path.starts_with(&format!("{seed}/"))
        || file.abs_path == Path::new(seed)
        || file.abs_path.starts_with(Path::new(seed))
}

fn normalized_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(ToString::to_string)
}

fn normalize_seed_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .map(|path| {
            path.trim_start_matches("./")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|path| !path.is_empty())
        .collect()
}

fn query_matches(path: &str, source: &str, query: &str) -> bool {
    let case_sensitive = query.chars().any(char::is_uppercase);
    if case_sensitive {
        return path.contains(query) || source.contains(query);
    }
    let query = query.to_ascii_lowercase();
    path.to_ascii_lowercase().contains(&query) || source.to_ascii_lowercase().contains(&query)
}

fn identifier_counts_cancellable(
    source: &str,
    language: &str,
    cancel: &CancelToken,
) -> Result<HashMap<String, usize>, CtxError> {
    let bytes = source.as_bytes();
    let mut counts = HashMap::new();
    let mut idx = 0usize;

    while idx < bytes.len() {
        if idx.is_multiple_of(1024) {
            cancel.check_cancelled()?;
        }
        if let Some(next_idx) = skip_non_code_span(bytes, idx, language) {
            idx = next_idx;
            continue;
        }

        if is_identifier_start(bytes[idx]) {
            let start = idx;
            idx += 1;
            while idx < bytes.len() && is_identifier_continue(bytes[idx]) {
                idx += 1;
            }
            let identifier = &source[start..idx];
            if !is_reference_stopword(identifier, language) {
                *counts.entry(identifier.to_string()).or_insert(0) += 1;
            }
            continue;
        }

        idx += 1;
    }

    Ok(counts)
}

#[cfg(fuzzing)]
#[doc(hidden)]
pub fn fuzz_identifier_counts(source: &str, language: &str) -> Result<usize, CtxError> {
    identifier_counts_cancellable(source, language, &CancelToken::never())
        .map(|counts| counts.len())
}

fn skip_non_code_span(bytes: &[u8], idx: usize, language: &str) -> Option<usize> {
    match language {
        "rust" => skip_rust_non_code_span(bytes, idx),
        "python" => skip_python_non_code_span(bytes, idx),
        "javascript" => skip_javascript_non_code_span(bytes, idx),
        _ => None,
    }
}

fn skip_rust_non_code_span(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes[idx..].starts_with(b"//") {
        return Some(skip_line(bytes, idx + 2));
    }
    if bytes[idx..].starts_with(b"/*") {
        return Some(skip_block_comment(bytes, idx + 2));
    }
    if let Some(end) = rust_raw_string_end(bytes, idx) {
        return Some(end);
    }
    if bytes[idx] == b'b'
        && idx + 1 < bytes.len()
        && let Some(end) = rust_raw_string_end(bytes, idx + 1)
    {
        return Some(end);
    }
    match bytes[idx] {
        b'"' => Some(skip_quoted(bytes, idx, b'"')),
        b'\'' => Some(skip_quoted(bytes, idx, b'\'')),
        _ => None,
    }
}

fn skip_python_non_code_span(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes[idx] == b'#' {
        return Some(skip_line(bytes, idx + 1));
    }
    python_string_end(bytes, idx)
}

fn skip_javascript_non_code_span(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes[idx..].starts_with(b"//") {
        return Some(skip_line(bytes, idx + 2));
    }
    if bytes[idx..].starts_with(b"/*") {
        return Some(skip_block_comment(bytes, idx + 2));
    }
    match bytes[idx] {
        b'"' => Some(skip_quoted(bytes, idx, b'"')),
        b'\'' => Some(skip_quoted(bytes, idx, b'\'')),
        b'`' => Some(skip_quoted(bytes, idx, b'`')),
        _ => None,
    }
}

fn skip_line(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() && bytes[idx] != b'\n' {
        idx += 1;
    }
    idx
}

fn skip_block_comment(bytes: &[u8], mut idx: usize) -> usize {
    while idx + 1 < bytes.len() {
        if bytes[idx..].starts_with(b"*/") {
            return idx + 2;
        }
        idx += 1;
    }
    bytes.len()
}

fn skip_quoted(bytes: &[u8], mut idx: usize, quote: u8) -> usize {
    idx += 1;
    while idx < bytes.len() {
        if bytes[idx] == b'\\' {
            idx = (idx + 2).min(bytes.len());
        } else if bytes[idx] == quote {
            return idx + 1;
        } else {
            idx += 1;
        }
    }
    bytes.len()
}

fn rust_raw_string_end(bytes: &[u8], idx: usize) -> Option<usize> {
    if bytes.get(idx).copied() != Some(b'r') {
        return None;
    }
    let mut marker_idx = idx + 1;
    while bytes.get(marker_idx).copied() == Some(b'#') {
        marker_idx += 1;
    }
    if bytes.get(marker_idx).copied() != Some(b'"') {
        return None;
    }

    let hashes = marker_idx - idx - 1;
    let mut scan = marker_idx + 1;
    while scan < bytes.len() {
        if bytes[scan] == b'"' && raw_hashes_match(bytes, scan + 1, hashes) {
            return Some(scan + 1 + hashes);
        }
        scan += 1;
    }
    Some(bytes.len())
}

fn raw_hashes_match(bytes: &[u8], idx: usize, hashes: usize) -> bool {
    idx + hashes <= bytes.len() && bytes[idx..idx + hashes].iter().all(|byte| *byte == b'#')
}

fn python_string_end(bytes: &[u8], idx: usize) -> Option<usize> {
    const PREFIXES: [&[u8]; 9] = [b"fr", b"rf", b"br", b"rb", b"ur", b"r", b"u", b"b", b"f"];

    for prefix in PREFIXES {
        if idx + prefix.len() >= bytes.len() {
            continue;
        }
        if bytes[idx..idx + prefix.len()].eq_ignore_ascii_case(prefix)
            && matches!(bytes[idx + prefix.len()], b'\'' | b'"')
        {
            return Some(skip_python_quoted(bytes, idx + prefix.len()));
        }
    }

    matches!(bytes[idx], b'\'' | b'"').then(|| skip_python_quoted(bytes, idx))
}

fn skip_python_quoted(bytes: &[u8], idx: usize) -> usize {
    let quote = bytes[idx];
    if idx + 2 < bytes.len() && bytes[idx + 1] == quote && bytes[idx + 2] == quote {
        let mut scan = idx + 3;
        while scan + 2 < bytes.len() {
            if bytes[scan] == quote && bytes[scan + 1] == quote && bytes[scan + 2] == quote {
                return scan + 3;
            }
            scan += 1;
        }
        bytes.len()
    } else {
        skip_quoted(bytes, idx, quote)
    }
}

fn is_reference_stopword(identifier: &str, language: &str) -> bool {
    !is_identifier(identifier)
        || identifier.len() < 3
        || language_keywords(language).contains(&identifier)
}

fn is_high_document_frequency(definer_count: usize, language_file_count: usize) -> bool {
    const HIGH_DF_MIN_FILES: usize = 4;
    const HIGH_DF_MAX_NUMERATOR: usize = 1;
    const HIGH_DF_MAX_DENOMINATOR: usize = 4;

    definer_count >= HIGH_DF_MIN_FILES
        && definer_count * HIGH_DF_MAX_DENOMINATOR > language_file_count * HIGH_DF_MAX_NUMERATOR
}

fn language_keywords(language: &str) -> &'static [&'static str] {
    match language {
        "rust" => &RUST_STOPWORDS,
        "python" => &PYTHON_STOPWORDS,
        "javascript" => &JAVASCRIPT_STOPWORDS,
        _ => &[],
    }
}

const RUST_STOPWORDS: [&str; 56] = [
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "if", "impl",
    "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref",
    "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield", "Result", "Option", "Some",
    "None", "Ok",
];

const PYTHON_STOPWORDS: [&str; 37] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "self", "try",
    "while", "with", "yield", "print",
];

const JAVASCRIPT_STOPWORDS: [&str; 47] = [
    "arguments",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "target",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

fn is_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    is_identifier_start(first) && bytes.all(is_identifier_continue)
}

fn is_identifier_start(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

fn is_identifier_continue(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

fn key_symbols(symbols: &[CodeSymbol]) -> Vec<CodeSymbol> {
    symbols.iter().take(MAX_SYMBOLS_PER_FILE).cloned().collect()
}

fn display_path(snapshot: &CatalogSnapshot, root_id: &str, rel_path: &str) -> String {
    if snapshot.roots.len() <= 1 {
        return rel_path.to_string();
    }
    format!("{root_id}/{rel_path}")
}

fn score_cmp(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions};
    use std::fs;

    fn temp_provider(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, FsCatalogProvider, CatalogSnapshot) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, content) in files {
            let full_path = dir.path().join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).expect("create dirs");
            }
            fs::write(full_path, content).expect("write fixture");
        }
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        (dir, provider, snapshot)
    }

    fn indexed_files(provider: &FsCatalogProvider, snapshot: &CatalogSnapshot) -> Vec<IndexedFile> {
        let analyses = analyze_files(provider, snapshot, None).expect("analysis");
        let mut files: Vec<_> = analyses
            .into_iter()
            .filter_map(|analysis| match analysis {
                FileAnalysisResult::Indexed(file) => Some(file),
                _ => None,
            })
            .collect();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        files
    }

    fn edge_weight(graph: &ReferenceGraph, files: &[IndexedFile], from: &str, to: &str) -> f64 {
        let from_idx = files.iter().position(|file| file.path == from).unwrap();
        let to_idx = files.iter().position(|file| file.path == to).unwrap();
        graph.edges[from_idx]
            .iter()
            .find_map(|(idx, weight)| (*idx == to_idx).then_some(*weight))
            .unwrap_or(0.0)
    }

    #[test]
    fn builds_reference_edges_from_identifier_occurrences() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub struct Target;\n"),
            (
                "caller.rs",
                "pub fn caller() { let _ = Target; let _ = Target; }\n",
            ),
            ("other.rs", "pub fn other() {}\n"),
        ]);
        let files = indexed_files(&provider, &snapshot);
        let graph = ReferenceGraph::build(&files);

        assert_eq!(edge_weight(&graph, &files, "caller.rs", "target.rs"), 2.0);
    }

    #[test]
    fn ignores_identifiers_inside_comments_and_strings() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub struct Target;\n"),
            (
                "caller.rs",
                r#"pub fn caller() { let _ = "Target"; /* Target */ } // Target
"#,
            ),
        ]);
        let files = indexed_files(&provider, &snapshot);
        let graph = ReferenceGraph::build(&files);

        assert_eq!(edge_weight(&graph, &files, "caller.rs", "target.rs"), 0.0);
    }

    #[test]
    fn ignores_high_document_frequency_symbols() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("one.rs", "pub struct CommonThing;\n"),
            ("two.rs", "pub struct CommonThing;\n"),
            ("three.rs", "pub struct CommonThing;\n"),
            ("four.rs", "pub struct CommonThing;\n"),
            ("caller.rs", "pub fn caller() { let _ = CommonThing; }\n"),
        ]);
        let files = indexed_files(&provider, &snapshot);
        let graph = ReferenceGraph::build(&files);
        let caller_idx = files
            .iter()
            .position(|file| file.path == "caller.rs")
            .unwrap();

        assert!(graph.edges[caller_idx].is_empty());
    }

    #[test]
    fn does_not_create_cross_language_edges_for_same_name() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("shared.js", "export class SharedThing {}\n"),
            ("caller.rs", "pub fn caller() { let _ = SharedThing; }\n"),
        ]);
        let files = indexed_files(&provider, &snapshot);
        let graph = ReferenceGraph::build(&files);

        assert_eq!(edge_weight(&graph, &files, "caller.rs", "shared.js"), 0.0);
    }

    #[test]
    fn same_language_consumer_reference_ranks_definer_higher() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub struct Target;\n"),
            (
                "caller.rs",
                "pub fn caller() { let _ = Target; let _ = Target; }\n",
            ),
            ("isolated.rs", "pub fn isolated() {}\n"),
        ]);
        let response = get_repo_map(
            &provider,
            &snapshot,
            &RepoMapRequest {
                query: Some("Target".to_string()),
                seed_paths: vec![PathBuf::from("caller.rs")],
                max_files: 3,
            },
        )
        .expect("repo map");

        let target = response
            .files
            .iter()
            .position(|file| file.path == "target.rs")
            .expect("target ranked");
        let caller = response
            .files
            .iter()
            .position(|file| file.path == "caller.rs")
            .expect("caller ranked");
        let target_score: f64 = response.files[target].score.parse().expect("target score");
        let caller_score: f64 = response.files[caller].score.parse().expect("caller score");

        assert!(target < caller);
        assert!(target_score > caller_score);
        assert!(response.totals.edges > 0);
    }

    #[test]
    fn pagerank_prefers_file_referenced_by_multiple_files() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub struct Target;\n"),
            ("caller_one.rs", "pub fn one() { let _ = Target; }\n"),
            ("caller_two.rs", "pub fn two() { let _ = Target; }\n"),
        ]);
        let response =
            get_repo_map(&provider, &snapshot, &RepoMapRequest::default()).expect("repo map");

        assert_eq!(response.files[0].path, "target.rs");
    }

    #[test]
    fn personalized_pagerank_biases_seed_files() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub struct Target;\n"),
            ("caller.rs", "pub fn caller() { let _ = Target; }\n"),
            ("isolated.rs", "pub fn isolated() {}\n"),
        ]);
        let response = get_repo_map(
            &provider,
            &snapshot,
            &RepoMapRequest {
                query: None,
                seed_paths: vec![PathBuf::from("isolated.rs")],
                max_files: 3,
            },
        )
        .expect("repo map");

        assert_eq!(response.files[0].path, "isolated.rs");
        assert_eq!(response.totals.seed_files, 1);
    }

    #[test]
    fn pre_cancelled_repo_map_returns_cancelled() {
        let (_dir, provider, snapshot) = temp_provider(&[("lib.rs", "pub fn alpha() {}\n")]);
        let token = CancelToken::new();
        token.cancel();

        let err =
            get_repo_map_cancellable(&provider, &snapshot, &RepoMapRequest::default(), &token)
                .expect_err("repo-map should cancel");
        assert!(matches!(err, CtxError::Cancelled));
    }

    #[test]
    fn repo_map_cancel_after_n_checks_is_deterministic() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub struct Target;\n"),
            ("caller.rs", "pub fn caller() { let _ = Target; }\n"),
        ]);
        let token = CancelToken::cancel_after_checks(3);

        let err =
            get_repo_map_cancellable(&provider, &snapshot, &RepoMapRequest::default(), &token)
                .expect_err("repo-map should cancel after injected check count");
        assert!(matches!(err, CtxError::Cancelled));
    }

    #[test]
    fn pagerank_checks_cancel_each_iteration() {
        let edges = vec![vec![(1, 1.0)], vec![(0, 1.0)]];
        let personalization = vec![0.5, 0.5];
        let token = CancelToken::cancel_after_checks(1);

        let err = page_rank_cancellable(&edges, &personalization, &token)
            .expect_err("pagerank should cancel on injected iteration check");
        assert!(matches!(err, CtxError::Cancelled));
    }
}
