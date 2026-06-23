//! PageRank-based repository map over the lightweight codemap.
//!
//! This module intentionally keeps the implementation pure Rust. It builds a
//! symbol-definition index from the existing top-level codemap, then uses
//! AST-derived reference nodes collected during that same parse pass to build a
//! deterministic sparse personalized PageRank file graph.
//!
//! Important limitation: reference edges are AST node-level name matches, not a
//! full scope/type resolver. Calls, imports, identifier/member references, and
//! type paths are matched by name against same-language top-level definitions;
//! aliases, re-exports, and multi-definition disambiguation remain out of scope.

mod analysis;
mod graph;
mod imports;
mod language;
mod query;
mod rank;
mod symbols;

#[cfg(test)]
mod tests;

pub(crate) use analysis::{IndexedFile, indexed_files_cancellable};
pub(crate) use graph::ReferenceGraph;
pub(crate) use imports::resolve_import_reference;

use crate::{
    cancel::CancelToken,
    codemap::CodeSymbol,
    graph::shared_reference_graph,
    models::{Diagnostic, NerveError},
    port::CatalogProvider,
    snapshot::CatalogSnapshot,
};
use analysis::{FileAnalysisResult, analyze_files_cancellable};
use query::{normalize_seed_paths, normalized_query, query_terms};
use rank::{DAMPING, ITERATIONS, page_rank_cancellable, personalization, score_cmp, seed_indices};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use symbols::key_symbols;

#[cfg(test)]
use analysis::analyze_files;
const DEFAULT_MAX_FILES: usize = 20;

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
    /// Key symbols, surfaced as names in the rendered map text. Not serialized
    /// into `structuredContent`: their names are already in the map text, and
    /// full signatures/members belong to `get_code_structure` on demand — a
    /// ranking tool should not re-encode codemap detail (aider emits text only).
    #[serde(default, skip_serializing)]
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
) -> Result<RepoMapResponse, NerveError> {
    // A freshly-built `Arc` is never `Arc::ptr_eq` to a memo entry, so this is a
    // deliberate memo miss (rebuild) — byte-identical output.
    get_repo_map_cancellable(
        provider,
        &Arc::new(snapshot.clone()),
        request,
        &CancelToken::never(),
    )
}

/// Build a PageRank repo-map from the catalog snapshot, checking `cancel` in
/// file analysis, graph construction, and each PageRank iteration.
pub fn get_repo_map_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &RepoMapRequest,
    cancel: &CancelToken,
) -> Result<RepoMapResponse, NerveError> {
    cancel.check_cancelled()?;
    let query = normalized_query(request.query.as_deref());
    let query_terms = query_terms(query.as_deref());
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

    let graph = shared_reference_graph(provider, snapshot, cancel)?;
    let seed_indices = seed_indices(&files, &seed_paths, &query_terms);
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
            "AST node-level references (imports/calls/type/name/member nodes) matched by same-language top-level symbol name; import paths that resolve to catalog files get higher weight; not a full scope/type/alias/re-export resolver"
                .to_string(),
    })
}
