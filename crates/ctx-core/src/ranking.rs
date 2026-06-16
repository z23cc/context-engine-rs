//! Shared lexical ranking primitives.
//!
//! The file-search ranker intentionally preserves its existing file-level
//! behavior. Semantic search builds a separate chunk-level corpus model on top of
//! the tokenization helpers here.

use crate::{models::CtxError, models::SearchRequest};
use regex::Regex;
use std::collections::{HashMap, HashSet};

const SCORE_SCALE: f64 = 1_000_000.0;
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

pub(crate) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug)]
pub(crate) enum ContentRankingQuery {
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
    pub(crate) fn new(pattern: &str, case_sensitive: bool, regex: bool, whole_word: bool) -> Self {
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

    pub(crate) fn stats_for_file(
        &self,
        path: &str,
        text: &str,
        occurrence_count: usize,
    ) -> FileRankingStats {
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
pub(crate) struct FileRankingStats {
    pub(crate) path: String,
    pub(crate) doc_len: usize,
    pub(crate) term_frequencies: HashMap<String, usize>,
    pub(crate) occurrence_count: usize,
}

pub(crate) fn content_file_scores(
    stats: &[FileRankingStats],
    query: &ContentRankingQuery,
) -> HashMap<String, i64> {
    match query {
        ContentRankingQuery::Bm25 { terms } => bm25_file_scores(stats, terms),
        ContentRankingQuery::TfDensity => tf_density_file_scores(stats),
    }
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

pub(crate) fn scaled_score(score: f64) -> i64 {
    (score * SCORE_SCALE).round() as i64
}

pub(crate) fn tokenize_query(pattern: &str, case_sensitive: bool) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut terms = Vec::new();
    for token in tokenize_text(pattern, case_sensitive) {
        if seen.insert(token.clone()) {
            terms.push(token);
        }
    }
    terms
}

pub(crate) fn tokenize_text(text: &str, case_sensitive: bool) -> Vec<String> {
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

pub(crate) fn normalized_doc_len(text: &str) -> usize {
    let token_len = tokenize_text(text, false).len();
    token_len.max(text.lines().count()).max(1)
}

pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|byte| *byte == 0)
}

/// Path filter applied to both buckets: an extension whitelist plus glob
/// include/exclude lists (ripgrep / Claude Code Grep parity).
pub(crate) struct EntryFilter {
    extensions: Vec<String>,
    include: Vec<Regex>,
    exclude: Vec<Regex>,
}

impl EntryFilter {
    pub(crate) fn build(request: &SearchRequest) -> Result<Self, CtxError> {
        let extensions = request
            .extensions
            .iter()
            .filter(|ext| !ext.is_empty())
            .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        Ok(Self {
            extensions,
            include: compile_globs(&request.include)?,
            exclude: compile_globs(&request.exclude)?,
        })
    }

    /// True if `rel_path` passes the extension whitelist, matches some include
    /// glob (when any are set), and matches no exclude glob.
    pub(crate) fn accepts(&self, rel_path: &str) -> bool {
        if !self.extensions.is_empty() {
            let ext = rel_path
                .rsplit_once('.')
                .map(|(_, ext)| ext.to_ascii_lowercase())
                .unwrap_or_default();
            if !self.extensions.contains(&ext) {
                return false;
            }
        }
        if !self.include.is_empty() && !self.include.iter().any(|re| re.is_match(rel_path)) {
            return false;
        }
        if self.exclude.iter().any(|re| re.is_match(rel_path)) {
            return false;
        }
        true
    }
}

pub(crate) fn compile_globs(globs: &[String]) -> Result<Vec<Regex>, CtxError> {
    globs
        .iter()
        .filter(|glob| !glob.is_empty())
        .map(|glob| Ok(Regex::new(&glob_to_regex(glob))?))
        .collect()
}

/// Translate a gitignore/ripgrep-style glob to an anchored regex. `*` matches
/// within a path segment, `**` across segments, `?` one non-slash char. A glob
/// without a `/` matches the basename at any depth (e.g. `*.rs`).
pub(crate) fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("(?s)^");
    if !glob.contains('/') {
        re.push_str("(?:.*/)?");
    }
    let chars: Vec<char> = glob.chars().collect();
    let mut idx = 0usize;
    while idx < chars.len() {
        match chars[idx] {
            '*' => {
                if chars.get(idx + 1) == Some(&'*') {
                    idx += 1;
                    if chars.get(idx + 1) == Some(&'/') {
                        idx += 1;
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '/' => re.push('/'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        idx += 1;
    }
    re.push('$');
    re
}
