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
    models::NerveError,
    port::CatalogProvider,
    repomap::{IndexedFile, indexed_files_cancellable, resolve_import_reference},
    snapshot::CatalogSnapshot,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

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

pub use call_hierarchy::{call_hierarchy, call_hierarchy_cancellable};
pub use definition::{goto_definition, goto_definition_cancellable};
pub use impact::{analyze_impact, analyze_impact_cancellable};
pub use read_symbol::{read_symbol, read_symbol_cancellable};
pub use references::{find_references, find_references_cancellable};
pub use referencing_symbols::{find_referencing_symbols, find_referencing_symbols_cancellable};
pub use symbol_search::{symbol_search, symbol_search_cancellable};

use source::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions};
    use std::fs;

    fn temp_provider(
        files: &[(&str, &str)],
    ) -> (tempfile::TempDir, FsCatalogProvider, CatalogSnapshot) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("dirs");
            }
            fs::write(full, content).expect("write");
        }
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        (dir, provider, snapshot)
    }

    fn request(symbol: &str) -> NavigateRequest {
        NavigateRequest {
            symbol: symbol.to_string(),
            language: None,
            include_definitions: false,
            confident_only: false,
            max_results: 200,
        }
    }

    fn calls(symbol: &str, direction: CallDirection) -> CallHierarchyRequest {
        CallHierarchyRequest {
            symbol: symbol.to_string(),
            direction,
            language: None,
            max_results: 200,
        }
    }

    fn symbol_query(query: &str) -> SymbolSearchRequest {
        SymbolSearchRequest {
            query: query.to_string(),
            language: None,
            kind: None,
            max_results: 200,
        }
    }

    fn impact_request(symbol: &str) -> ImpactAnalysisRequest {
        ImpactAnalysisRequest {
            symbol: symbol.to_string(),
            path: None,
            language: None,
            kind: None,
            max_depth: 2,
            max_results: 200,
            confident_only: false,
        }
    }

    fn referencing_request(symbol: &str) -> FindReferencingSymbolsRequest {
        FindReferencingSymbolsRequest {
            symbol: symbol.to_string(),
            path: None,
            language: None,
            kind: None,
            confident_only: false,
            context_lines: 1,
            max_results: 200,
        }
    }

    fn read_request(symbol: &str) -> ReadSymbolRequest {
        ReadSymbolRequest {
            symbol: symbol.to_string(),
            path: None,
            language: None,
            kind: None,
            include_body: true,
            max_matches: 20,
        }
    }

    #[test]
    fn symbol_search_finds_partial_terms_and_ranks_symbols() {
        let (_dir, provider, snapshot) = temp_provider(&[(
            "payments.rs",
            "pub struct PaymentGateway;\npub fn process_payment() {}\n",
        )]);

        let response = symbol_search(&provider, &snapshot, &symbol_query("pay gate")).expect("nav");

        assert_eq!(response.total, 1);
        let found = &response.matches[0];
        assert_eq!(found.name, "PaymentGateway");
        assert_eq!(found.kind, "class");
        assert_eq!(found.language, "rust");
        assert_eq!(found.text.as_deref(), Some("pub struct PaymentGateway;"));
        assert_eq!(found.matched_terms, vec!["pay", "gate"]);
    }

    #[test]
    fn symbol_search_splits_acronym_boundaries() {
        let (_dir, provider, snapshot) = temp_provider(&[(
            "server.rs",
            "pub struct HTTPServer;\nimpl HTTPServer { pub fn start() {} }\n",
        )]);

        let response =
            symbol_search(&provider, &snapshot, &symbol_query("http server")).expect("nav");

        assert_eq!(response.total, 1);
        assert_eq!(response.matches[0].name, "HTTPServer");
        assert_eq!(response.matches[0].matched_terms, vec!["http", "server"]);
    }

    #[test]
    fn symbol_search_filters_by_kind_language_and_honors_zero_limit() {
        let (_dir, provider, snapshot) = temp_provider(&[
            (
                "payments.rs",
                "pub struct PaymentGateway;\npub fn process_payment() {}\n",
            ),
            ("payments.js", "export function process_payment() {}\n"),
        ]);
        let mut request = symbol_query("payment");
        request.language = Some("rust".to_string());
        request.kind = Some("function".to_string());
        request.max_results = 1;

        let response = symbol_search(&provider, &snapshot, &request).expect("nav");
        assert_eq!(response.total, 1);
        assert_eq!(response.matches[0].name, "process_payment");
        assert_eq!(response.matches[0].language, "rust");

        request.max_results = 0;
        let zero = symbol_search(&provider, &snapshot, &request).expect("nav");
        assert_eq!(zero.total, 1);
        assert!(zero.matches.is_empty());
        assert!(zero.truncated);
    }

    #[test]
    fn goto_definition_finds_symbol_with_signature_and_snippet() {
        let (_dir, provider, snapshot) = temp_provider(&[(
            "lib.rs",
            "pub struct Widget;\npub fn make_widget() -> Widget { Widget }\n",
        )]);
        let response = goto_definition(&provider, &snapshot, &request("make_widget")).expect("nav");
        assert_eq!(response.total, 1);
        let def = &response.definitions[0];
        assert_eq!(def.path, "lib.rs");
        assert_eq!(def.line, 2);
        assert_eq!(def.kind, "function");
        assert!(
            def.signature
                .as_deref()
                .unwrap_or_default()
                .contains("make_widget")
        );
        assert_eq!(
            def.text.as_deref(),
            Some("pub fn make_widget() -> Widget { Widget }")
        );
    }

    #[test]
    fn analyze_impact_returns_direct_and_recursive_dependents() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helper.rs", "pub fn helper() {}\n"),
            ("middle.rs", "pub fn middle() {\n    helper();\n}\n"),
            ("top.rs", "pub fn top() {\n    middle();\n}\n"),
        ]);

        let response =
            analyze_impact(&provider, &snapshot, &impact_request("helper")).expect("impact");

        assert_eq!(response.definitions.len(), 1);
        assert_eq!(response.definitions[0].path, "helper.rs");
        let impacted: Vec<(&str, usize, &str)> = response
            .impacted
            .iter()
            .map(|item| (item.symbol.as_str(), item.depth, item.via_symbol.as_str()))
            .collect();
        assert!(impacted.contains(&("middle", 1, "helper")));
        assert!(impacted.contains(&("top", 2, "middle")));
        assert!(
            response
                .impacted
                .iter()
                .all(|item| item.confidence == Confidence::High)
        );
    }

    #[test]
    fn analyze_impact_honors_max_depth() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helper.rs", "pub fn helper() {}\n"),
            ("middle.rs", "pub fn middle() { helper(); }\n"),
            ("top.rs", "pub fn top() { middle(); }\n"),
        ]);
        let mut request = impact_request("helper");
        request.max_depth = 1;

        let response = analyze_impact(&provider, &snapshot, &request).expect("impact");

        assert!(response.impacted.iter().any(|item| item.symbol == "middle"));
        assert!(!response.impacted.iter().any(|item| item.symbol == "top"));
    }

    #[test]
    fn analyze_impact_truncates_results() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helper.rs", "pub fn helper() {}\n"),
            ("a.rs", "pub fn a() { helper(); }\n"),
            ("b.rs", "pub fn b() { helper(); }\n"),
        ]);
        let mut request = impact_request("helper");
        request.max_results = 1;

        let response = analyze_impact(&provider, &snapshot, &request).expect("impact");

        assert_eq!(response.total, 2);
        assert_eq!(response.impacted.len(), 1);
        assert!(response.truncated);
    }

    #[test]
    fn analyze_impact_excludes_self_recursive_seed() {
        let (_dir, provider, snapshot) =
            temp_provider(&[("helper.rs", "pub fn helper() {\n    helper();\n}\n")]);

        let response =
            analyze_impact(&provider, &snapshot, &impact_request("helper")).expect("impact");

        assert_eq!(response.definitions.len(), 1);
        assert!(response.impacted.is_empty());
    }

    #[test]
    fn analyze_impact_keeps_recursive_ambiguity_low_confidence() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helper.rs", "pub fn helper() {}\n"),
            ("real.rs", "pub fn middle() { helper(); }\n"),
            ("other.rs", "pub fn middle() {}\n"),
            ("caller.rs", "pub fn caller() { middle(); }\n"),
        ]);

        let response =
            analyze_impact(&provider, &snapshot, &impact_request("helper")).expect("impact");
        let caller = response
            .impacted
            .iter()
            .find(|item| item.symbol == "caller")
            .expect("caller impact");
        assert_eq!(caller.confidence, Confidence::Low);

        let mut confident = impact_request("helper");
        confident.confident_only = true;
        let filtered = analyze_impact(&provider, &snapshot, &confident).expect("impact");
        assert!(!filtered.impacted.iter().any(|item| item.symbol == "caller"));
    }

    #[test]
    fn find_referencing_symbols_returns_enclosing_symbols_with_context() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn helper() {}\n"),
            (
                "caller.rs",
                "pub fn alpha() -> usize {\n    let x = 1;\n    helper();\n    x\n}\n\npub fn beta() {\n    helper();\n}\n",
            ),
        ]);

        let response =
            find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
                .expect("referencing symbols");

        assert_eq!(response.definitions.len(), 1);
        assert_eq!(response.definitions[0].column, 8);
        assert_eq!(response.definition_count, 1);
        assert_eq!(response.total, 2);
        let alpha = response
            .referencing_symbols
            .iter()
            .find(|item| item.symbol == "alpha")
            .expect("alpha reference");
        assert_eq!(alpha.path, "caller.rs");
        assert_eq!(alpha.line, 1);
        assert_eq!(alpha.column, 8);
        assert_eq!(alpha.reference_line, 3);
        assert_eq!(alpha.reference_column, 5);
        assert_eq!(alpha.confidence, Confidence::High);
        assert_eq!(alpha.reference_text.as_deref(), Some("helper();"));
        let context = alpha.reference_context.as_deref().expect("context");
        assert!(context.contains("2:     let x = 1;"));
        assert!(context.contains("3:     helper();"));
        assert!(context.contains("4:     x"));
    }

    #[test]
    fn find_referencing_symbols_marks_low_confidence_and_filters() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("a.rs", "pub fn helper() {}\n"),
            ("b.rs", "pub fn helper() {}\n"),
            ("user.rs", "pub fn run() { helper(); }\n"),
        ]);

        let response =
            find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
                .expect("referencing symbols");
        assert_eq!(response.definition_count, 2);
        let run = response
            .referencing_symbols
            .iter()
            .find(|item| item.symbol == "run")
            .expect("run reference");
        assert_eq!(run.confidence, Confidence::Low);

        let mut confident = referencing_request("helper");
        confident.confident_only = true;
        let filtered = find_referencing_symbols(&provider, &snapshot, &confident)
            .expect("referencing symbols");
        assert!(filtered.referencing_symbols.is_empty());
    }

    #[test]
    fn find_referencing_symbols_truncates_and_can_omit_context() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helper.rs", "pub fn helper() {}\n"),
            ("a.rs", "pub fn a() { helper(); }\n"),
            ("b.rs", "pub fn b() { helper(); }\n"),
        ]);
        let mut request = referencing_request("helper");
        request.max_results = 1;
        request.context_lines = 0;

        let response =
            find_referencing_symbols(&provider, &snapshot, &request).expect("referencing symbols");

        assert_eq!(response.total, 2);
        assert_eq!(response.referencing_symbols.len(), 1);
        assert!(response.truncated);
        assert!(response.referencing_symbols[0].reference_context.is_none());
    }

    #[test]
    fn find_referencing_symbols_keeps_same_line_reference_columns() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helper.rs", "pub fn helper() {}\n"),
            ("caller.rs", "pub fn caller() { helper(); helper(); }\n"),
        ]);

        let response =
            find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
                .expect("referencing symbols");

        let columns: Vec<usize> = response
            .referencing_symbols
            .iter()
            .map(|item| item.reference_column)
            .collect();
        assert_eq!(response.total, 2);
        assert_eq!(columns, vec![19, 29]);
    }

    #[test]
    fn find_referencing_symbols_caps_context_lines_in_navigation_api() {
        let (_dir, provider, snapshot) = temp_provider(&[(
            "lib.rs",
            "pub fn helper() {}\n\npub fn caller() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    helper();\n    let f = 6;\n    let g = 7;\n    let h = 8;\n    let i = 9;\n    let j = 10;\n}\n",
        )]);
        let mut request = referencing_request("helper");
        request.context_lines = 100;

        let response =
            find_referencing_symbols(&provider, &snapshot, &request).expect("referencing symbols");

        assert_eq!(response.context_lines, 5);
        let context = response.referencing_symbols[0]
            .reference_context
            .as_deref()
            .expect("context");
        assert_eq!(context.lines().count(), 11);
        assert!(context.contains("4:     let a = 1;"));
        assert!(context.contains("14:     let j = 10;"));
        assert!(!context.contains("3: pub fn caller()"));
        assert!(!context.contains("15: }"));
    }

    #[test]
    fn find_referencing_symbols_keeps_duplicate_relative_paths_from_multiple_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_a = dir.path().join("root-a");
        let root_b = dir.path().join("root-b");
        fs::create_dir_all(&root_a).expect("root-a");
        fs::create_dir_all(&root_b).expect("root-b");
        let source = "pub fn helper() {}\n\npub fn caller() {\n    helper();\n}\n";
        fs::write(root_a.join("lib.rs"), source).expect("root-a lib");
        fs::write(root_b.join("lib.rs"), source).expect("root-b lib");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![root_a, root_b]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");

        let response =
            find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
                .expect("referencing symbols");

        let display_paths: Vec<&str> = response
            .referencing_symbols
            .iter()
            .map(|item| item.display_path.as_str())
            .collect();
        assert_eq!(response.definition_count, 2);
        assert_eq!(response.total, 2);
        assert_eq!(display_paths, vec!["root-0/lib.rs", "root-1/lib.rs"]);
    }

    #[test]
    fn read_symbol_returns_body_for_single_exact_match() {
        let (_dir, provider, snapshot) = temp_provider(&[(
            "lib.rs",
            "pub fn alpha() -> usize {\n    let value = 41;\n    value + 1\n}\n\npub fn beta() {}\n",
        )]);

        let response = read_symbol(&provider, &snapshot, &read_request("alpha")).expect("symbol");

        assert_eq!(response.total, 1);
        assert_eq!(response.matches.len(), 1);
        let body = response.body.expect("body");
        assert_eq!(body.path, "lib.rs");
        assert_eq!(body.start_line, 1);
        assert_eq!(body.end_line, 4);
        assert!(body.content.contains("let value = 41"));
        assert!(!body.content.contains("pub fn beta"));
    }

    #[test]
    fn read_symbol_returns_candidates_without_body_when_ambiguous() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("a.rs", "pub fn alpha() {}\n"),
            ("b.rs", "pub fn alpha() {}\n"),
        ]);

        let response = read_symbol(&provider, &snapshot, &read_request("alpha")).expect("symbol");

        assert_eq!(response.total, 2);
        assert!(response.body.is_none());
        assert_eq!(response.matches.len(), 2);
        assert_eq!(response.matches[0].path, "a.rs");
        assert_eq!(response.matches[1].path, "b.rs");
    }

    #[test]
    fn read_symbol_path_scope_resolves_duplicate_name() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("src/a.rs", "pub fn alpha() -> usize { 1 }\n"),
            ("src/b.rs", "pub fn alpha() -> usize { 2 }\n"),
        ]);
        let mut request = read_request("alpha");
        request.path = Some("src/a.rs".to_string());

        let response = read_symbol(&provider, &snapshot, &request).expect("symbol");

        assert_eq!(response.total, 1);
        let body = response.body.expect("body");
        assert_eq!(body.path, "src/a.rs");
        assert!(body.content.contains("{ 1 }"));
    }

    #[test]
    fn read_symbol_can_return_location_only() {
        let (_dir, provider, snapshot) = temp_provider(&[("lib.rs", "pub fn alpha() {}\n")]);
        let mut request = read_request("alpha");
        request.include_body = false;

        let response = read_symbol(&provider, &snapshot, &request).expect("symbol");

        assert_eq!(response.total, 1);
        assert!(response.body.is_none());
        assert_eq!(response.matches[0].path, "lib.rs");
    }

    #[test]
    fn read_symbol_public_api_clamps_zero_max_matches() {
        let (_dir, provider, snapshot) = temp_provider(&[("lib.rs", "pub fn alpha() {}\n")]);
        let mut request = read_request("alpha");
        request.max_matches = 0;

        let response = read_symbol(&provider, &snapshot, &request).expect("symbol");

        assert_eq!(response.total, 1);
        assert_eq!(response.matches.len(), 1);
        assert!(response.body.is_some());
    }

    #[test]
    fn find_references_locates_call_sites_with_snippet() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
            (
                "caller.rs",
                "pub fn caller() -> usize { make_target() + make_target() }\n",
            ),
        ]);
        let response = find_references(&provider, &snapshot, &request("make_target")).expect("nav");
        assert!(response.total >= 1, "expected at least one reference");
        assert!(response.references.iter().all(|r| r.path == "caller.rs"));
        assert!(
            response.references[0]
                .text
                .as_deref()
                .unwrap_or_default()
                .contains("make_target")
        );
    }

    #[test]
    fn ambiguous_name_marks_low_confidence_and_confident_only_filters() {
        // `helper` defined in two unrelated files; a third file references it
        // without importing either -> ambiguous, low confidence.
        let (_dir, provider, snapshot) = temp_provider(&[
            ("a.rs", "pub fn helper() {}\n"),
            ("b.rs", "pub fn helper() {}\n"),
            ("user.rs", "pub fn run() { helper(); }\n"),
        ]);
        let response = find_references(&provider, &snapshot, &request("helper")).expect("nav");
        assert_eq!(response.definition_count, 2, "two definitions of helper");
        let user_ref = response
            .references
            .iter()
            .find(|r| r.path == "user.rs")
            .expect("user ref");
        assert_eq!(user_ref.confidence, Confidence::Low);

        // confident_only drops the ambiguous user.rs reference.
        let mut req = request("helper");
        req.confident_only = true;
        let filtered = find_references(&provider, &snapshot, &req).expect("nav");
        assert!(!filtered.references.iter().any(|r| r.path == "user.rs"));
    }

    #[test]
    fn unambiguous_name_is_high_confidence() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn only_one() {}\n"),
            ("caller.rs", "pub fn run() { only_one(); }\n"),
        ]);
        let response = find_references(&provider, &snapshot, &request("only_one")).expect("nav");
        assert_eq!(response.definition_count, 1);
        assert!(
            response
                .references
                .iter()
                .all(|r| r.confidence == Confidence::High)
        );
    }

    #[test]
    fn find_references_can_include_definitions() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
            ("caller.rs", "pub fn caller() { make_target(); }\n"),
        ]);
        let mut req = request("make_target");
        req.include_definitions = true;
        let response = find_references(&provider, &snapshot, &req).expect("nav");
        assert_eq!(response.definitions.len(), 1);
        assert_eq!(response.definitions[0].path, "target.rs");
    }

    #[test]
    fn find_references_uses_embedded_reference_language() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
            (
                "README.md",
                "```rust\npub fn example() -> usize { make_target() }\n```\n",
            ),
        ]);
        let mut req = request("make_target");
        req.language = Some("rust".to_string());
        let response = find_references(&provider, &snapshot, &req).expect("nav");
        let doc_ref = response
            .references
            .iter()
            .find(|reference| reference.path == "README.md")
            .expect("README reference");
        assert_eq!(doc_ref.language, "rust");
        assert_eq!(doc_ref.line, 2);
    }

    #[test]
    fn language_filter_excludes_other_languages() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("a.rs", "pub fn shared() {}\n"),
            ("b.js", "export function shared() {}\n"),
        ]);
        let mut req = request("shared");
        req.language = Some("rust".to_string());
        let response = goto_definition(&provider, &snapshot, &req).expect("nav");
        assert_eq!(response.total, 1);
        assert_eq!(response.definitions[0].language, "rust");
    }

    #[test]
    fn unknown_symbol_returns_empty() {
        let (_dir, provider, snapshot) = temp_provider(&[("lib.rs", "pub fn alpha() {}\n")]);
        let response = goto_definition(&provider, &snapshot, &request("missing")).expect("nav");
        assert_eq!(response.total, 0);
        assert!(response.definitions.is_empty());
    }

    #[test]
    fn max_results_truncates_and_flags() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("a.rs", "pub fn dup() {}\n"),
            ("b.rs", "pub fn dup() {}\n"),
            ("c.rs", "pub fn dup() {}\n"),
        ]);
        let mut req = request("dup");
        req.max_results = 2;
        let response = goto_definition(&provider, &snapshot, &req).expect("nav");
        assert_eq!(response.total, 3);
        assert_eq!(response.definitions.len(), 2);
        assert!(response.truncated);
    }

    #[test]
    fn call_hierarchy_incoming_resolves_enclosing_caller() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
            (
                "caller.rs",
                "pub fn outer() -> usize {\n    make_target()\n}\n",
            ),
        ]);
        let response = call_hierarchy(
            &provider,
            &snapshot,
            &calls("make_target", CallDirection::Incoming),
        )
        .expect("calls");
        assert!(response.incoming.iter().any(|e| e.symbol == "outer"));
        assert!(response.outgoing.is_empty());
    }

    #[test]
    fn call_hierarchy_incoming_skips_embedded_markdown_references() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
            (
                "README.md",
                "```rust\nfn docs() -> usize {\n    make_target()\n}\n```\n",
            ),
        ]);
        let response = call_hierarchy(
            &provider,
            &snapshot,
            &calls("make_target", CallDirection::Incoming),
        )
        .expect("calls");

        assert!(response.incoming.is_empty());
    }

    #[test]
    fn call_hierarchy_outgoing_resolves_callees() {
        let (_dir, provider, snapshot) = temp_provider(&[
            ("helpers.rs", "pub fn helper_a() {}\npub fn helper_b() {}\n"),
            (
                "main.rs",
                "pub fn driver() {\n    helper_a();\n    helper_b();\n}\n",
            ),
        ]);
        let response = call_hierarchy(
            &provider,
            &snapshot,
            &calls("driver", CallDirection::Outgoing),
        )
        .expect("calls");
        let callees: Vec<&str> = response
            .outgoing
            .iter()
            .map(|e| e.symbol.as_str())
            .collect();
        assert!(callees.contains(&"helper_a"));
        assert!(callees.contains(&"helper_b"));
        assert!(response.incoming.is_empty());
    }
}
