use super::args::lenient_usize;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct FindReferencingSymbolsArgs {
    pub(super) symbol: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(default)]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) confident_only: bool,
    #[serde(
        default = "default_reference_context_lines",
        deserialize_with = "lenient_usize"
    )]
    pub(super) context_lines: usize,
    #[serde(
        default = "default_referencing_symbols_max_results",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_results: usize,
}

impl FindReferencingSymbolsArgs {
    pub(super) fn into_request(self) -> crate::navigate::FindReferencingSymbolsRequest {
        crate::navigate::FindReferencingSymbolsRequest {
            symbol: self.symbol,
            path: self.path,
            language: self.language,
            kind: self.kind,
            confident_only: self.confident_only,
            context_lines: self.context_lines.min(5),
            max_results: self.max_results.max(1),
        }
    }
}

fn default_reference_context_lines() -> usize {
    1
}

fn default_referencing_symbols_max_results() -> usize {
    200
}
