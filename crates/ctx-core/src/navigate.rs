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
    cancel::CancelToken, models::CtxError, port::CatalogProvider,
    repomap::indexed_files_cancellable, snapshot::CatalogSnapshot,
};
use serde::{Deserialize, Serialize};

/// Caveat surfaced on every navigation response so callers (and models) know the
/// results are syntactic name matches, not compiler-accurate resolution.
const NAV_NOTE: &str = "syntax-level name match across the catalog; not a scope/type resolver, so results may include unrelated same-name symbols and miss aliases or re-exports";

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
    /// Maximum locations returned per bucket.
    pub max_results: usize,
}

/// One definition site for a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolLocation {
    pub path: String,
    pub display_path: String,
    pub line: usize,
    pub kind: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// One reference site for a symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceLocation {
    pub path: String,
    pub display_path: String,
    pub line: usize,
    pub kind: String,
    pub language: String,
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
    pub total: usize,
    pub truncated: bool,
    pub note: String,
}

fn language_matches(filter: Option<&str>, language: &str) -> bool {
    filter.is_none_or(|wanted| wanted == language)
}

/// Collect every definition site of `symbol`, sorted deterministically.
fn collect_definitions<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
    cancel: &CancelToken,
) -> Result<Vec<SymbolLocation>, CtxError> {
    let files = indexed_files_cancellable(provider, snapshot, cancel)?;
    let mut hits = Vec::new();
    for file in &files {
        if !language_matches(request.language.as_deref(), &file.language) {
            continue;
        }
        for symbol in &file.symbols {
            if symbol.name == request.symbol {
                hits.push(SymbolLocation {
                    path: file.path.clone(),
                    display_path: file.display_path.clone(),
                    line: symbol.line,
                    kind: symbol.kind.clone(),
                    language: file.language.clone(),
                    signature: symbol.signature.clone(),
                });
            }
        }
    }
    hits.sort_by(|a, b| {
        a.display_path
            .cmp(&b.display_path)
            .then(a.line.cmp(&b.line))
            .then(a.kind.cmp(&b.kind))
    });
    Ok(hits)
}

/// Find all definitions of `symbol` across the catalog.
pub fn goto_definition<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
) -> Result<DefinitionResponse, CtxError> {
    goto_definition_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Cancellable [`goto_definition`].
pub fn goto_definition_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
    cancel: &CancelToken,
) -> Result<DefinitionResponse, CtxError> {
    let mut definitions = collect_definitions(provider, snapshot, request, cancel)?;
    let total = definitions.len();
    let truncated = total > request.max_results;
    definitions.truncate(request.max_results);
    Ok(DefinitionResponse {
        symbol: request.symbol.clone(),
        definitions,
        total,
        truncated,
        note: NAV_NOTE.to_string(),
    })
}

/// Find all references to `symbol` across the catalog.
pub fn find_references<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
) -> Result<ReferencesResponse, CtxError> {
    find_references_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Cancellable [`find_references`].
pub fn find_references_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
    cancel: &CancelToken,
) -> Result<ReferencesResponse, CtxError> {
    let files = indexed_files_cancellable(provider, snapshot, cancel)?;
    let mut references = Vec::new();
    for file in &files {
        if !language_matches(request.language.as_deref(), &file.language) {
            continue;
        }
        for reference in &file.references {
            if reference.name == request.symbol {
                references.push(ReferenceLocation {
                    path: file.path.clone(),
                    display_path: file.display_path.clone(),
                    line: reference.line,
                    kind: reference.kind.clone(),
                    language: file.language.clone(),
                });
            }
        }
    }
    references.sort_by(|a, b| {
        a.display_path
            .cmp(&b.display_path)
            .then(a.line.cmp(&b.line))
            .then(a.kind.cmp(&b.kind))
    });
    let total = references.len();
    let truncated = total > request.max_results;
    references.truncate(request.max_results);

    let definitions = if request.include_definitions {
        let mut defs = collect_definitions(provider, snapshot, request, cancel)?;
        defs.truncate(request.max_results);
        defs
    } else {
        Vec::new()
    };

    Ok(ReferencesResponse {
        symbol: request.symbol.clone(),
        references,
        definitions,
        total,
        truncated,
        note: NAV_NOTE.to_string(),
    })
}

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
            max_results: 200,
        }
    }

    #[test]
    fn goto_definition_finds_symbol_with_signature() {
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
    }

    #[test]
    fn find_references_locates_call_sites() {
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
}
