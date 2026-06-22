use super::args::{default_true, lenient_usize};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub(super) struct ReadSymbolArgs {
    pub(super) symbol: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(default)]
    pub(super) kind: Option<String>,
    #[serde(default = "default_true")]
    pub(super) include_body: bool,
    #[serde(
        default = "default_read_symbol_max_matches",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_matches: usize,
}

impl ReadSymbolArgs {
    pub(super) fn into_request(self) -> crate::navigate::ReadSymbolRequest {
        crate::navigate::ReadSymbolRequest {
            symbol: self.symbol,
            path: self.path,
            language: self.language,
            kind: self.kind,
            include_body: self.include_body,
            max_matches: self.max_matches.max(1),
        }
    }
}

fn default_read_symbol_max_matches() -> usize {
    20
}
