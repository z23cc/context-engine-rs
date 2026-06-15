//! Immutable catalog snapshot.

use crate::models::{CatalogEntry, Diagnostic, RootRef};
use serde::{Deserialize, Serialize};

/// A point-in-time view of allowed roots and cataloged files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSnapshot {
    pub generation: u64,
    pub roots: Vec<RootRef>,
    pub entries: Vec<CatalogEntry>,
    pub diagnostics: Vec<Diagnostic>,
}

impl CatalogSnapshot {
    #[must_use]
    pub fn empty(generation: u64) -> Self {
        Self {
            generation,
            roots: Vec::new(),
            entries: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}
