//! Deterministic query-to-context builder.

use crate::{
    CancelToken, CatalogEntry, CatalogProvider, ContentSearchMatch, CtxError, LineRange,
    PathSearchMatch, ReadFileRequest, RepoMapRequest, SearchMode, SearchRequest, Selection,
    SelectionMode, WorkspaceContextInclude, WorkspaceContextRequest, WorkspaceContextResponse,
    count_tokens, get_repo_map_cancellable, read_file, search_snapshot_cancellable,
    workspace_context_for_selection,
};
#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
use crate::{SemanticSearchMode, SemanticSearchRequest};
use crate::{repomap::RepoMapResponse, selection::SelectionKey};
use serde::{Deserialize, Serialize};

mod reference_expansion;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

const DEFAULT_MAX_FILES: usize = 20;
const SEARCH_WEIGHT: f64 = 0.55;
const REPOMAP_WEIGHT: f64 = 0.35;
const PATH_WEIGHT: f64 = 0.10;
const SEARCH_SEMANTIC_WEIGHT: f64 = 0.30;
const REPOMAP_SEMANTIC_WEIGHT: f64 = 0.25;
const SEMANTIC_WEIGHT: f64 = 0.35;
const SLICE_RADIUS: usize = 2;

/// Request for the `build_context` primitive and MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BuildContextRequest {
    pub query: String,
    pub token_budget: usize,
    pub max_files: Option<usize>,
    /// Optional files that seed the repo-map's personalized PageRank, biasing
    /// selection toward these files and their references.
    #[serde(default)]
    pub seed_paths: Vec<PathBuf>,
}

/// Response from `build_context`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextResponse {
    /// Assembled context text. Not serialized into `structuredContent`: it is
    /// already the tool's `content[].text`, so emitting it twice would double
    /// the payload. The manifest (ranking/selection/tokens) stays structured.
    #[serde(default, skip_serializing)]
    pub context: String,
    pub manifest: BuildContextManifest,
}

/// Manifest describing ranking, selected modes, exclusions, and token use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextManifest {
    pub query: String,
    pub token_budget: usize,
    pub token_used: usize,
    pub included: Vec<BuildContextIncludedFile>,
    pub excluded: Vec<BuildContextExcludedFile>,
}

/// Included file selected by the builder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextIncludedFile {
    pub path: String,
    pub display_path: String,
    pub mode: String,
    pub tokens: usize,
    pub score: String,
}

/// Ranked file not included in the built context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextExcludedFile {
    pub path: String,
    pub display_path: String,
    pub score: String,
    pub reason: String,
}

/// Build context using search + repo-map ranking and greedy token allocation.
pub fn build_context<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    request: &BuildContextRequest,
) -> Result<BuildContextResponse, CtxError> {
    build_context_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Build context with cooperative cancellation.
pub fn build_context_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    request: &BuildContextRequest,
    cancel: &CancelToken,
) -> Result<BuildContextResponse, CtxError> {
    cancel.check_cancelled()?;
    let max_files = request.max_files.unwrap_or(DEFAULT_MAX_FILES).max(1);
    let search = search_snapshot_cancellable(
        provider,
        snapshot,
        &SearchRequest {
            pattern: request.query.clone(),
            mode: SearchMode::Both,
            regex: false,
            max_results: max_files.saturating_mul(4).max(50),
            context_lines: SLICE_RADIUS,
            ..SearchRequest::default()
        },
        cancel,
    )?;
    cancel.check_cancelled()?;
    let repo_map = get_repo_map_cancellable(
        provider,
        snapshot,
        &RepoMapRequest {
            query: Some(request.query.clone()),
            seed_paths: request.seed_paths.clone(),
            max_files: max_files.saturating_mul(4).max(20),
        },
        cancel,
    )?;
    cancel.check_cancelled()?;
    let semantic_scores =
        semantic_candidate_scores(provider, snapshot, request, max_files, cancel)?;

    let ranked = rank_candidates(
        provider,
        snapshot,
        request.query.as_str(),
        &search,
        &repo_map,
        &semantic_scores,
    );
    let (mut selection, mut excluded) =
        allocate_selection(provider, snapshot, &ranked, request.token_budget, max_files)?;
    excluded.extend(reference_expansion::expand_reference_codemap_selection(
        provider,
        snapshot,
        &mut selection,
        request.token_budget,
        cancel,
    )?);
    remove_selected_exclusions(&mut excluded, &selection);
    let mut workspace = workspace_context_for_selection(
        provider,
        snapshot,
        &selection,
        &WorkspaceContextRequest {
            include: vec![
                WorkspaceContextInclude::FileMap,
                WorkspaceContextInclude::Contents,
            ],
            instructions: None,
        },
    )?;
    if selection.files.is_empty() && workspace.tokens.total_tokens > request.token_budget {
        workspace.context.clear();
        workspace.tokens.total_tokens = 0;
        workspace.tokens.file_map_tokens = 0;
    }
    let included = included_manifest(&workspace, &ranked);

    Ok(BuildContextResponse {
        context: workspace.context,
        manifest: BuildContextManifest {
            query: request.query.clone(),
            token_budget: request.token_budget,
            token_used: workspace.tokens.total_tokens,
            included,
            excluded,
        },
    })
}

#[derive(Debug)]
struct Candidate<'a> {
    entry: &'a CatalogEntry,
    display_path: String,
    score: f64,
    hit_lines: BTreeSet<usize>,
}

fn rank_candidates<'a, P: CatalogProvider>(
    provider: &P,
    snapshot: &'a crate::CatalogSnapshot,
    query: &str,
    search: &crate::SearchResponse,
    repo_map: &RepoMapResponse,
    semantic_scores: &BTreeMap<String, f64>,
) -> Vec<Candidate<'a>> {
    let entries_by_path = snapshot
        .entries
        .iter()
        .map(|entry| (entry.rel_path.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut builders = BTreeMap::<String, CandidateBuilder>::new();

    for hit in &search.path_matches {
        add_path_hit(&mut builders, hit);
    }
    for hit in &search.content_matches {
        add_content_hit(&mut builders, hit);
    }
    for file in &repo_map.files {
        builders.entry(file.path.clone()).or_default().repo_score =
            file.score.parse::<f64>().unwrap_or(0.0);
    }
    for (path, score) in semantic_scores {
        builders.entry(path.clone()).or_default().semantic_score = score.max(0.0);
    }
    for entry in &snapshot.entries {
        let path_score = path_relevance(&entry.rel_path, query);
        if path_score > 0.0 {
            builders
                .entry(entry.rel_path.clone())
                .or_default()
                .path_score = path_score;
        }
    }

    let max_search = builders
        .values()
        .map(|builder| builder.search_score)
        .fold(0.0, f64::max);
    let max_repo = builders
        .values()
        .map(|builder| builder.repo_score)
        .fold(0.0, f64::max);
    let max_semantic = builders
        .values()
        .map(|builder| builder.semantic_score)
        .fold(0.0, f64::max);
    let has_semantic = !semantic_scores.is_empty();

    let mut candidates = builders
        .into_iter()
        .filter_map(|(path, builder)| {
            let entry = entries_by_path.get(&path)?;
            let search_score = normalize(builder.search_score, max_search);
            let repo_score = normalize(builder.repo_score, max_repo);
            let path_score = builder.path_score;
            let semantic_score = normalize(builder.semantic_score, max_semantic);
            let score = fused_score(
                search_score,
                repo_score,
                semantic_score,
                path_score,
                has_semantic,
            );
            (score > 0.0).then(|| Candidate {
                entry,
                display_path: provider.display_path(&entry.abs_path),
                score,
                hit_lines: builder.hit_lines,
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.entry.rel_path.cmp(&right.entry.rel_path))
    });
    candidates
}

#[derive(Debug, Default)]
struct CandidateBuilder {
    search_score: f64,
    repo_score: f64,
    semantic_score: f64,
    path_score: f64,
    hit_lines: BTreeSet<usize>,
}

fn add_path_hit(builders: &mut BTreeMap<String, CandidateBuilder>, hit: &PathSearchMatch) {
    builders.entry(hit.path.clone()).or_default().search_score += hit.score.max(0) as f64 * 0.25;
}

fn add_content_hit(builders: &mut BTreeMap<String, CandidateBuilder>, hit: &ContentSearchMatch) {
    let builder = builders.entry(hit.path.clone()).or_default();
    builder.search_score += hit.score.max(0) as f64 + 2_000_000.0;
    builder.hit_lines.insert(hit.line);
}

fn normalize(value: f64, max: f64) -> f64 {
    if max > 0.0 { value / max } else { 0.0 }
}

fn fused_score(search: f64, repo: f64, semantic: f64, path: f64, has_semantic: bool) -> f64 {
    if has_semantic {
        search * SEARCH_SEMANTIC_WEIGHT
            + repo * REPOMAP_SEMANTIC_WEIGHT
            + semantic * SEMANTIC_WEIGHT
            + path * PATH_WEIGHT
    } else {
        search.mul_add(SEARCH_WEIGHT, repo * REPOMAP_WEIGHT) + path * PATH_WEIGHT
    }
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
fn semantic_candidate_scores<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    request: &BuildContextRequest,
    max_files: usize,
    cancel: &CancelToken,
) -> Result<BTreeMap<String, f64>, CtxError> {
    let Some(index) = provider.semantic_index() else {
        return Ok(BTreeMap::new());
    };
    let request = SemanticSearchRequest {
        query: request.query.clone(),
        mode: SemanticSearchMode::Hybrid,
        max_results: max_files.saturating_mul(4).max(1),
        rerank: false,
    };
    match index.search_if_ready(provider, snapshot, &request, cancel) {
        Ok(Some(response)) => Ok(semantic_scores_by_path(response)),
        Ok(None) => Ok(BTreeMap::new()),
        Err(CtxError::Cancelled) => Err(CtxError::Cancelled),
        Err(_) => Ok(BTreeMap::new()),
    }
}

#[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
fn semantic_candidate_scores<P: CatalogProvider + Sync>(
    _provider: &P,
    _snapshot: &crate::CatalogSnapshot,
    _request: &BuildContextRequest,
    _max_files: usize,
    _cancel: &CancelToken,
) -> Result<BTreeMap<String, f64>, CtxError> {
    Ok(BTreeMap::new())
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
fn semantic_scores_by_path(response: crate::SemanticSearchResponse) -> BTreeMap<String, f64> {
    let mut scores = BTreeMap::<String, f64>::new();
    for result in response.results {
        if result.score.is_finite() {
            scores
                .entry(result.path)
                .and_modify(|score| *score = score.max(result.score))
                .or_insert(result.score);
        }
    }
    scores
}

fn path_relevance(path: &str, query: &str) -> f64 {
    let path = path.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();
    let terms = query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|term| !term.is_empty())
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return 0.0;
    }
    let matched = terms.iter().filter(|term| path.contains(**term)).count();
    matched as f64 / terms.len() as f64
}

fn allocate_selection<P: CatalogProvider>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    ranked: &[Candidate<'_>],
    token_budget: usize,
    max_files: usize,
) -> Result<(Selection, Vec<BuildContextExcludedFile>), CtxError> {
    let mut selection = Selection::default();
    let mut excluded = Vec::new();

    for (idx, candidate) in ranked.iter().enumerate() {
        if selection.files.len() >= max_files {
            excluded.extend(
                ranked[idx..]
                    .iter()
                    .map(|candidate| excluded_file(candidate, "max_files")),
            );
            break;
        }

        let mut included = false;
        for mode in candidate_modes(provider, candidate, token_budget)? {
            let mut next_selection = selection.clone();
            next_selection
                .files
                .insert(selection_key(candidate.entry), mode);
            let workspace = workspace_context_for_selection(
                provider,
                snapshot,
                &next_selection,
                &WorkspaceContextRequest {
                    include: vec![
                        WorkspaceContextInclude::FileMap,
                        WorkspaceContextInclude::Contents,
                    ],
                    instructions: None,
                },
            )?;
            if workspace.tokens.total_tokens <= token_budget {
                selection = next_selection;
                included = true;
                break;
            }
        }

        if !included {
            excluded.push(excluded_file(candidate, "over_budget"));
        }
    }

    Ok((selection, excluded))
}

fn remove_selected_exclusions(excluded: &mut Vec<BuildContextExcludedFile>, selection: &Selection) {
    let selected_paths = selection
        .files
        .keys()
        .map(|key| key.path.as_str())
        .collect::<BTreeSet<_>>();
    excluded.retain(|file| !selected_paths.contains(file.path.as_str()));
}

fn candidate_modes<P: CatalogProvider>(
    provider: &P,
    candidate: &Candidate<'_>,
    token_budget: usize,
) -> Result<Vec<SelectionMode>, CtxError> {
    let full_tokens = full_content_tokens(provider, candidate.entry)?;
    let ranges = hit_line_ranges(&candidate.hit_lines);
    let codemap_supported = provider
        .code_symbols_for_path(&candidate.entry.abs_path, &candidate.entry.rel_path)?
        .ok()
        .flatten()
        .is_some();

    let mut modes = vec![SelectionMode::Full];
    if codemap_supported {
        modes.push(SelectionMode::CodemapOnly);
    }
    let huge_with_hits = full_tokens > token_budget.saturating_div(2) && !ranges.is_empty();
    if (huge_with_hits || !codemap_supported) && !ranges.is_empty() {
        modes.push(SelectionMode::Slices(ranges));
    }
    Ok(modes)
}

fn full_content_tokens<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
) -> Result<usize, CtxError> {
    let response = read_file(
        provider,
        &ReadFileRequest {
            path: entry.abs_path.clone(),
            start_line: None,
            end_line: None,
            limit: None,
            snap: None,
        },
    )?;
    Ok(count_tokens(&response.content))
}

fn hit_line_ranges(lines: &BTreeSet<usize>) -> Vec<LineRange> {
    lines
        .iter()
        .take(3)
        .map(|line| LineRange {
            start_line: line.saturating_sub(SLICE_RADIUS).max(1),
            end_line: line.saturating_add(SLICE_RADIUS),
        })
        .collect()
}

fn included_manifest(
    workspace: &WorkspaceContextResponse,
    ranked: &[Candidate<'_>],
) -> Vec<BuildContextIncludedFile> {
    let score_by_path = ranked
        .iter()
        .map(|candidate| (candidate.entry.rel_path.as_str(), candidate.score))
        .collect::<BTreeMap<_, _>>();
    workspace
        .tokens
        .files
        .iter()
        .map(|file| BuildContextIncludedFile {
            path: file.path.clone(),
            display_path: file.display_path.clone(),
            mode: file.mode.clone(),
            tokens: file.token_count,
            score: format_score(*score_by_path.get(file.path.as_str()).unwrap_or(&0.0)),
        })
        .collect()
}

fn excluded_file(candidate: &Candidate<'_>, reason: &str) -> BuildContextExcludedFile {
    BuildContextExcludedFile {
        path: candidate.entry.rel_path.clone(),
        display_path: candidate.display_path.clone(),
        score: format_score(candidate.score),
        reason: reason.to_string(),
    }
}

fn selection_key(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

fn format_score(score: f64) -> String {
    format!("{score:.6}")
}
