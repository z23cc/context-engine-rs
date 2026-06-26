//! Relocated provider-dependent unit tests for deterministic symbol navigation
//! (`navigate/mod.rs` and `navigate/trace_path.rs`).
//!
//! These used to live in `nerve-core`'s in-src `#[cfg(test)]` modules but moved
//! out because they drive `nerve_fs::FsCatalogProvider` (the `dev-dependencies`
//! back-edge forbids constructing it in an in-src test, which would compile
//! `nerve-core` twice — "multiple versions of crate `nerve_core`"). They use only
//! the public navigation API, so they need no `test_internals` re-export.

use nerve_core::navigate::Confidence;
use nerve_core::*;
use nerve_fs::{FsCatalogProvider, ScanOptions};
use std::fs;

// ---- navigate/mod.rs ----

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

    let response = symbol_search(&provider, &snapshot, &symbol_query("http server")).expect("nav");

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

    let response = analyze_impact(&provider, &snapshot, &impact_request("helper")).expect("impact");

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

    let response = analyze_impact(&provider, &snapshot, &impact_request("helper")).expect("impact");

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

    let response = analyze_impact(&provider, &snapshot, &impact_request("helper")).expect("impact");
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

    let response = find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
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

    let response = find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
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
    let filtered =
        find_referencing_symbols(&provider, &snapshot, &confident).expect("referencing symbols");
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

    let response = find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
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

    let response = find_referencing_symbols(&provider, &snapshot, &referencing_request("helper"))
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

// ---- navigate/trace_path.rs ----

fn provider_for(dir: &std::path::Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    )
}

fn trace_request(from: &str, to: &str) -> TracePathRequest {
    TracePathRequest {
        from: from.to_string(),
        to: to.to_string(),
        max_depth: 8,
        language: None,
    }
}

#[test]
fn finds_a_call_chain_through_an_intermediate() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn top() { middle(); }\npub fn middle() { leaf(); }\npub fn leaf() {}\n",
    )
    .expect("write");
    let provider = provider_for(dir.path());
    let snapshot = provider.snapshot_arc().expect("snapshot");

    let response = trace_path_cancellable(
        &provider,
        &snapshot,
        &trace_request("top", "leaf"),
        &CancelToken::never(),
    )
    .expect("trace");

    assert!(response.found, "top -> middle -> leaf should be found");
    let chain: Vec<&str> = response
        .path
        .iter()
        .map(|step| step.symbol.as_str())
        .collect();
    assert_eq!(chain, vec!["top", "middle", "leaf"]);
}

#[test]
fn unreachable_target_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn top() { middle(); }\npub fn middle() {}\npub fn island() {}\n",
    )
    .expect("write");
    let provider = provider_for(dir.path());
    let snapshot = provider.snapshot_arc().expect("snapshot");

    let response = trace_path_cancellable(
        &provider,
        &snapshot,
        &trace_request("top", "island"),
        &CancelToken::never(),
    )
    .expect("trace");

    assert!(!response.found);
    assert!(response.path.is_empty());
}

#[test]
fn missing_source_symbol_is_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn only() {}\n").expect("write");
    let provider = provider_for(dir.path());
    let snapshot = provider.snapshot_arc().expect("snapshot");

    let response = trace_path_cancellable(
        &provider,
        &snapshot,
        &trace_request("ghost", "only"),
        &CancelToken::never(),
    )
    .expect("trace");

    assert!(!response.found);
}
