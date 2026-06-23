//! Deterministic repository **scout**: query → compact `path:line-range`
//! citations.
//!
//! This is the FastContext idea (microsoft/fastcontext) realized without a model:
//! instead of an LLM sub-agent grepping the tree, it reuses [`build_context`]'s
//! exact ranking ([`super::ranked_candidates`] — BM25 search + repo-map PageRank +
//! path relevance) and turns each ranked file's content-hit lines into clustered
//! line ranges. The main agent calls it to find WHERE code lives and gets focused
//! citations back instead of whole files — keeping its context window clean — and
//! because the engine is deterministic, no trained exploration model is needed.
//!
//! Files ranked only by graph centrality / path relevance (no content hits) are
//! returned "file-level" (empty `ranges`): they are relevant but have no single
//! anchor line.

use super::{Candidate, ranked_candidates};
use crate::{CancelToken, CatalogProvider, CatalogSnapshot, NerveError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Default number of files a scout returns citations for.
const DEFAULT_SCOUT_FILES: usize = 12;
/// Cap on line-range slices reported per file (keeps the citation list compact).
const MAX_RANGES_PER_FILE: usize = 5;

/// Request for the `scout` primitive and MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ScoutRequest {
    /// What to find — natural language or identifiers.
    pub query: String,
    /// Maximum files to return citations for (default `DEFAULT_SCOUT_FILES`).
    pub max_files: Option<usize>,
    /// Optional files that seed the repo-map's personalized PageRank, biasing the
    /// ranking toward these files and their references.
    #[serde(default)]
    pub seed_paths: Vec<PathBuf>,
}

/// Response from `scout`: the query plus ranked citations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutResponse {
    pub query: String,
    pub citations: Vec<ScoutCitation>,
}

/// One ranked file with the line ranges most relevant to the query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutCitation {
    /// Workspace-relative path.
    pub path: String,
    /// Display path (root-id-prefixed in multi-root workspaces).
    pub display_path: String,
    /// Fused relevance score (formatted, matches `build_context`).
    pub score: String,
    /// Relevant line ranges (clustered from content hits). Empty when the file is
    /// relevant only by graph centrality / path — a "file-level" citation.
    pub ranges: Vec<ScoutRange>,
}

/// An inclusive 1-based line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutRange {
    pub start: usize,
    pub end: usize,
}

/// Scout the snapshot for `request.query` (convenience wrapper over
/// [`scout_cancellable`] with a never-cancelling token).
pub fn scout<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ScoutRequest,
) -> Result<ScoutResponse, NerveError> {
    scout_cancellable(
        provider,
        &Arc::new(snapshot.clone()),
        request,
        &CancelToken::never(),
    )
}

/// Scout with cooperative cancellation: rank candidates with the shared
/// `build_context` ranking, then emit the top `max_files` as compact citations.
pub fn scout_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &ScoutRequest,
    cancel: &CancelToken,
) -> Result<ScoutResponse, NerveError> {
    let max_files = request.max_files.unwrap_or(DEFAULT_SCOUT_FILES).max(1);
    let ranked = ranked_candidates(
        provider,
        snapshot,
        request.query.as_str(),
        &request.seed_paths,
        max_files,
        cancel,
    )?;
    let citations = ranked
        .iter()
        .take(max_files)
        .map(citation_from_candidate)
        .collect();
    Ok(ScoutResponse {
        query: request.query.clone(),
        citations,
    })
}

/// Render one ranked candidate as a citation: cluster its content-hit lines into
/// widened ranges (or leave `ranges` empty for a file-level, graph-only hit).
fn citation_from_candidate(candidate: &Candidate<'_>) -> ScoutCitation {
    ScoutCitation {
        path: candidate.entry.rel_path.clone(),
        display_path: candidate.display_path.clone(),
        score: super::format_score(candidate.score),
        ranges: cluster_ranges(&candidate.hit_lines),
    }
}

/// Cluster sorted hit lines into ranges: consecutive hits within
/// `2 * SLICE_RADIUS` lines merge into one cluster, each widened by `SLICE_RADIUS`
/// on both sides (clamped to line 1). Capped at [`MAX_RANGES_PER_FILE`].
fn cluster_ranges(hit_lines: &BTreeSet<usize>) -> Vec<ScoutRange> {
    let radius = super::SLICE_RADIUS;
    let merge_gap = radius.saturating_mul(2);
    let mut ranges = Vec::new();
    let mut iter = hit_lines.iter().copied();
    let Some(first) = iter.next() else {
        return ranges;
    };
    let (mut start, mut prev) = (first, first);
    for line in iter {
        if line.saturating_sub(prev) > merge_gap {
            ranges.push(widen(start, prev, radius));
            start = line;
        }
        prev = line;
    }
    ranges.push(widen(start, prev, radius));
    ranges.truncate(MAX_RANGES_PER_FILE);
    ranges
}

/// Widen `[start, end]` by `radius` on both sides, clamping the start to line 1.
fn widen(start: usize, end: usize, radius: usize) -> ScoutRange {
    ScoutRange {
        start: start.saturating_sub(radius).max(1),
        end: end.saturating_add(radius),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_ranges_empty_is_file_level() {
        assert!(cluster_ranges(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn cluster_ranges_merges_adjacent_and_splits_distant() {
        // SLICE_RADIUS = 2 → merge_gap = 4. Lines 10,11,13 merge (gaps ≤4); 40 splits.
        let lines: BTreeSet<usize> = [10, 11, 13, 40].into_iter().collect();
        let ranges = cluster_ranges(&lines);
        assert_eq!(
            ranges,
            vec![
                ScoutRange { start: 8, end: 15 },  // [10-2 .. 13+2]
                ScoutRange { start: 38, end: 42 }, // [40-2 .. 40+2]
            ]
        );
    }

    #[test]
    fn widen_clamps_start_to_line_one() {
        assert_eq!(widen(1, 1, 2), ScoutRange { start: 1, end: 3 });
    }

    #[test]
    fn cluster_ranges_caps_at_max_per_file() {
        // Many distant single-line hits → one range each, capped at MAX_RANGES_PER_FILE.
        let lines: BTreeSet<usize> = (0..MAX_RANGES_PER_FILE + 3).map(|i| 1 + i * 100).collect();
        assert_eq!(cluster_ranges(&lines).len(), MAX_RANGES_PER_FILE);
    }
}
