use serde::{Deserialize, Serialize};

use super::{
    PATH_WEIGHT, REPOMAP_SEMANTIC_WEIGHT, REPOMAP_WEIGHT, SEARCH_SEMANTIC_WEIGHT, SEARCH_WEIGHT,
    SEMANTIC_WEIGHT,
};

/// Deterministic per-signal contribution trace for build_context ranking.
///
/// Values are fixed-precision strings so structured output stays byte-stable
/// across platforms and serde float formatting changes. The four signal fields
/// are rounded weighted contributions; `total` matches the authoritative
/// ranking score exposed as the entry's existing `score` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextScoreBreakdown {
    pub search: String,
    pub repo_map: String,
    pub semantic: String,
    pub path: String,
    pub total: String,
    pub source: String,
    pub semantic_enabled: bool,
}

/// One budget trial for a candidate file/mode during greedy allocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextAllocationAttempt {
    pub mode: String,
    pub total_tokens: usize,
    pub accepted: bool,
}

/// Explainable allocation result for one ranked candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextAllocationTrace {
    pub path: String,
    pub display_path: String,
    pub score: String,
    pub score_breakdown: BuildContextScoreBreakdown,
    pub attempts: Vec<BuildContextAllocationAttempt>,
    pub result: String,
    pub reason: String,
}

impl BuildContextScoreBreakdown {
    pub(super) fn from_normalized(
        search: f64,
        repo_map: f64,
        semantic: f64,
        path: f64,
        total: f64,
        semantic_enabled: bool,
    ) -> Self {
        let (search_weight, repo_weight, semantic_weight) = if semantic_enabled {
            (
                SEARCH_SEMANTIC_WEIGHT,
                REPOMAP_SEMANTIC_WEIGHT,
                SEMANTIC_WEIGHT,
            )
        } else {
            (SEARCH_WEIGHT, REPOMAP_WEIGHT, 0.0)
        };
        Self {
            search: format_score(search * search_weight),
            repo_map: format_score(repo_map * repo_weight),
            semantic: format_score(semantic * semantic_weight),
            path: format_score(path * PATH_WEIGHT),
            total: format_score(total),
            source: "ranked".to_string(),
            semantic_enabled,
        }
    }

    pub fn zero() -> Self {
        Self {
            search: format_score(0.0),
            repo_map: format_score(0.0),
            semantic: format_score(0.0),
            path: format_score(0.0),
            total: format_score(0.0),
            source: "not_ranked".to_string(),
            semantic_enabled: false,
        }
    }
}

fn format_score(score: f64) -> String {
    format!("{score:.6}")
}
