//! Deterministic, syntax-level symbol navigation.
//!
//! Built on the same tree-sitter tag extraction that powers the repo-map, this
//! is **search-based** navigation in the sense of ctags / tree-sitter code
//! navigation: a symbol name is matched against the definition and reference
//! tags collected across the catalog. It is **not** a scope/type resolver, so
//! results may include false positives (an unrelated symbol of the same name)
//! and miss aliased or re-exported bindings. The upside is that it needs no
//! language server and no external process, runs over all 11 supported
//! languages, and is fully deterministic — the right tradeoff for an embeddable,
//! reproducible backend (the same choice GitHub makes for code navigation at
//! scale).

use crate::{
    cancel::CancelToken,
    codemap::{block_span, containing_block_span},
    graph::{DefinitionNameIndex, shared_definition_index, shared_indexed_files},
    models::NerveError,
    port::CatalogProvider,
    repomap::{IndexedFile, resolve_import_reference},
    snapshot::CatalogSnapshot,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

/// Wrap a borrowed snapshot in a fresh owned `Arc` for the non-cancellable
/// public entry points. A freshly-built `Arc` is never `Arc::ptr_eq` to a memo
/// entry, so it is a deliberate memo miss (rebuild) — byte-identical output.
fn owned_arc(snapshot: &CatalogSnapshot) -> Arc<CatalogSnapshot> {
    Arc::new(snapshot.clone())
}

/// Caveat surfaced on every navigation response so callers (and models) know the
/// results are syntactic name matches, not compiler-accurate resolution.
const NAV_NOTE: &str = "syntax-level name match across the catalog; not a scope/type resolver, so results may include unrelated same-name symbols and miss aliases or re-exports";

/// Request for `symbol_search`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SymbolSearchRequest {
    /// Partial/fuzzy symbol query. Terms are matched case-insensitively against
    /// names, kinds, signatures, members, and paths.
    pub query: String,
    /// Restrict results to this display language (e.g. `rust`, `typescript`,
    /// `tsx`). `None` searches every indexed language.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional case-insensitive symbol-kind filter (e.g. `function`, `struct`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Maximum matches returned. `0` is allowed and returns only totals.
    pub max_results: usize,
}

/// One fuzzy symbol-search match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSearchMatch {
    pub name: String,
    pub path: String,
    pub display_path: String,
    pub line: usize,
    pub kind: String,
    pub language: String,
    pub score: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Trimmed source line at `line`, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub matched_terms: Vec<String>,
}

/// Response for `symbol_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolSearchResponse {
    pub query: String,
    pub matches: Vec<SymbolSearchMatch>,
    pub total: usize,
    pub truncated: bool,
    pub note: String,
}

/// Request for `goto_definition` / `find_references`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct NavigateRequest {
    /// Exact symbol name to look up (case-sensitive).
    pub symbol: String,
    /// Restrict results to this display language (e.g. `rust`, `typescript`,
    /// `tsx`). `None` searches every language.
    #[serde(default)]
    pub language: Option<String>,
    /// `find_references` only: also return the symbol's definitions.
    #[serde(default)]
    pub include_definitions: bool,
    /// `find_references` only: drop low-confidence (ambiguous name-only) hits.
    #[serde(default)]
    pub confident_only: bool,
    /// Maximum locations returned per bucket.
    pub max_results: usize,
}

/// How trustworthy a syntactic reference match is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// The name is unambiguous (single definition), the reference shares a file
    /// with a definition, or its file imports a defining file.
    High,
    /// Name-only match while multiple definitions of that name exist elsewhere.
    Low,
}

/// One definition site for a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolLocation {
    pub path: String,
    pub display_path: String,
    pub line: usize,
    #[serde(default)]
    pub column: usize,
    pub kind: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Trimmed source line at `line`, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// One reference site for a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub path: String,
    pub display_path: String,
    pub line: usize,
    #[serde(default)]
    pub column: usize,
    pub kind: String,
    pub language: String,
    /// Trimmed source line at `line`, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether this name match is unambiguous / import-backed (`high`) or a
    /// name-only match while the name is defined in multiple places (`low`).
    pub confidence: Confidence,
}

/// Response for `goto_definition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionResponse {
    pub symbol: String,
    pub definitions: Vec<SymbolLocation>,
    pub total: usize,
    pub truncated: bool,
    pub note: String,
}

/// Response for `find_references`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencesResponse {
    pub symbol: String,
    pub references: Vec<ReferenceLocation>,
    /// Populated only when `include_definitions` is set.
    pub definitions: Vec<SymbolLocation>,
    /// Number of definition sites of this name (in scope); >1 means the name is
    /// ambiguous, so low-confidence references may belong to a different symbol.
    pub definition_count: usize,
    pub total: usize,
    pub truncated: bool,
    pub note: String,
}

/// Request for `find_referencing_symbols`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FindReferencingSymbolsRequest {
    /// Exact target symbol name (case-sensitive).
    pub symbol: String,
    /// Optional file or directory scope for the target definition(s).
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict target definitions and references to this display language.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional case-insensitive target symbol-kind filter.
    #[serde(default)]
    pub kind: Option<String>,
    /// Drop low-confidence name-only hits when true.
    #[serde(default)]
    pub confident_only: bool,
    /// Lines before/after the reference line to include in `reference_context`.
    pub context_lines: usize,
    /// Maximum referencing-symbol entries returned.
    pub max_results: usize,
}

/// One enclosing symbol that references the requested target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferencingSymbol {
    pub symbol: String,
    pub path: String,
    pub display_path: String,
    pub line: usize,
    #[serde(default)]
    pub column: usize,
    pub kind: String,
    pub language: String,
    pub reference_line: usize,
    #[serde(default)]
    pub reference_column: usize,
    pub reference_kind: String,
    pub confidence: Confidence,
    /// Trimmed source line for the enclosing symbol declaration, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Trimmed source line at the exact reference, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_text: Option<String>,
    /// Numbered source lines around the reference, when requested and available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_context: Option<String>,
}

/// Response for `find_referencing_symbols`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindReferencingSymbolsResponse {
    pub symbol: String,
    pub definitions: Vec<SymbolLocation>,
    pub referencing_symbols: Vec<ReferencingSymbol>,
    pub definition_count: usize,
    pub total: usize,
    pub truncated: bool,
    pub context_lines: usize,
    pub note: String,
}

/// Request for `analyze_impact`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ImpactAnalysisRequest {
    /// Exact symbol name to analyze (case-sensitive).
    pub symbol: String,
    /// Optional file or directory scope for the seed definition(s).
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict seed definitions and references to this display language.
    #[serde(default)]
    pub language: Option<String>,
    /// Optional case-insensitive seed symbol-kind filter.
    #[serde(default)]
    pub kind: Option<String>,
    /// Maximum reverse-dependency depth. `1` means direct dependents only.
    pub max_depth: usize,
    /// Maximum impacted symbols returned.
    pub max_results: usize,
    /// Drop low-confidence name-only matches when true.
    #[serde(default)]
    pub confident_only: bool,
}

/// One symbol whose body references the requested symbol or another impacted symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactSymbol {
    pub symbol: String,
    pub path: String,
    pub display_path: String,
    pub line: usize,
    #[serde(default)]
    pub column: usize,
    pub kind: String,
    pub language: String,
    pub depth: usize,
    pub via_symbol: String,
    pub reference_line: usize,
    #[serde(default)]
    pub reference_column: usize,
    pub reference_kind: String,
    pub confidence: Confidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Response for `analyze_impact`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactAnalysisResponse {
    pub symbol: String,
    pub definitions: Vec<SymbolLocation>,
    pub impacted: Vec<ImpactSymbol>,
    pub total: usize,
    pub truncated: bool,
    pub max_depth: usize,
    pub note: String,
}

/// Request for `read_symbol`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReadSymbolRequest {
    /// Exact symbol name to read (case-sensitive).
    pub symbol: String,
    /// Optional file or directory scope. Accepts root-relative paths and, in
    /// multi-root workspaces, root-id-prefixed display paths.
    #[serde(default)]
    pub path: Option<String>,
    /// Restrict results to this display language (e.g. `rust`, `typescript`).
    #[serde(default)]
    pub language: Option<String>,
    /// Optional case-insensitive symbol-kind filter (e.g. `function`, `class`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Include the enclosing source block only when exactly one match exists.
    #[serde(default = "default_true")]
    pub include_body: bool,
    /// Maximum candidate matches returned when ambiguous.
    pub max_matches: usize,
}

/// Source body for one exact symbol match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadSymbolBody {
    pub path: String,
    pub display_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub kind: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub content: String,
}

/// Response for `read_symbol`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadSymbolResponse {
    pub symbol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<ReadSymbolBody>,
    pub matches: Vec<SymbolLocation>,
    pub total: usize,
    pub truncated: bool,
    pub note: String,
}

fn default_true() -> bool {
    true
}

/// Direction of a `call_hierarchy` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    /// Callers of the symbol (who references it, by enclosing definition).
    Incoming,
    /// Callees of the symbol (what its body references, resolved to definitions).
    Outgoing,
    /// Both directions.
    Both,
}

/// Request for `call_hierarchy`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CallHierarchyRequest {
    pub symbol: String,
    pub direction: CallDirection,
    #[serde(default)]
    pub language: Option<String>,
    pub max_results: usize,
}

/// One related symbol in a call hierarchy (a caller for incoming, a callee for
/// outgoing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEdge {
    /// The related symbol's name.
    pub symbol: String,
    pub path: String,
    pub display_path: String,
    pub line: usize,
    #[serde(default)]
    pub column: usize,
    pub kind: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Response for `call_hierarchy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallHierarchyResponse {
    pub symbol: String,
    /// Callers (populated for `incoming`/`both`).
    pub incoming: Vec<CallEdge>,
    /// Callees (populated for `outgoing`/`both`).
    pub outgoing: Vec<CallEdge>,
    pub note: String,
}

fn language_matches(filter: Option<&str>, language: &str) -> bool {
    filter.is_none_or(|wanted| wanted == language)
}

/// Lazily reads file sources (once per path) to supply line snippets and full
/// source text for tree-sitter block-span resolution.
fn sort_locations(locations: &mut [SymbolLocation]) {
    locations.sort_by(|a, b| {
        a.display_path
            .cmp(&b.display_path)
            .then(a.line.cmp(&b.line))
            .then(a.kind.cmp(&b.kind))
    });
}
mod call_hierarchy;
mod definition;
mod impact;
mod read_symbol;
mod references;
mod referencing_symbols;
mod source;
mod symbol_search;
mod trace_path;

pub use call_hierarchy::{call_hierarchy, call_hierarchy_cancellable};
pub use definition::{goto_definition, goto_definition_cancellable};
pub use impact::{analyze_impact, analyze_impact_cancellable};
pub use read_symbol::{read_symbol, read_symbol_cancellable};
pub use references::{find_references, find_references_cancellable};
pub use referencing_symbols::{find_referencing_symbols, find_referencing_symbols_cancellable};
pub use symbol_search::{symbol_search, symbol_search_cancellable};
pub use trace_path::{
    PathStep, TracePathRequest, TracePathResponse, trace_path, trace_path_cancellable,
};

use source::*;
