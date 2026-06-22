use super::args::lenient_usize;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct ImpactAnalysisArgs {
    pub(super) symbol: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(default)]
    pub(super) kind: Option<String>,
    #[serde(
        default = "default_impact_max_depth",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_depth: usize,
    #[serde(
        default = "default_impact_max_results",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_results: usize,
    #[serde(default)]
    pub(super) confident_only: bool,
}

impl ImpactAnalysisArgs {
    pub(super) fn into_request(self) -> crate::navigate::ImpactAnalysisRequest {
        crate::navigate::ImpactAnalysisRequest {
            symbol: self.symbol,
            path: self.path,
            language: self.language,
            kind: self.kind,
            max_depth: self.max_depth.max(1),
            max_results: self.max_results.max(1),
            confident_only: self.confident_only,
        }
    }
}

fn default_impact_max_depth() -> usize {
    2
}

fn default_impact_max_results() -> usize {
    200
}
