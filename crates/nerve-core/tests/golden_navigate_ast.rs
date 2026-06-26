//! Golden snapshot tests for the navigate tools (`goto_definition`,
//! `find_references`, `call_hierarchy`) and AST tools (`ast_search`,
//! `ast_edit`).
//!
//! Each test controls its own fixture content via a fresh tempdir, so the
//! snapshots are independent of the shared `tests/fixtures/` tree and of each
//! other.

use nerve_core::{RootPolicy, handle_tool_call};
use nerve_fs::{FsCatalogProvider, ScanOptions};
use serde_json::json;
use std::fs;

fn make_provider(root: &std::path::Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![root.to_path_buf()]).expect("root policy"),
        ScanOptions::default(),
    )
}

// ---------------------------------------------------------------------------
// Navigate: goto_definition
// ---------------------------------------------------------------------------

/// Rust source containing struct Point, trait Shape, and an impl block.
const SHAPES_RS: &str = "\
pub struct Point {\n\
    pub x: f32,\n\
    pub y: f32,\n\
}\n\
\n\
pub trait Shape {\n\
    fn area(&self) -> f32;\n\
}\n\
\n\
impl Shape for Point {\n\
    fn area(&self) -> f32 {\n\
        0.0\n\
    }\n\
}\n";

#[test]
fn golden_goto_definition() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("shapes.rs"), SHAPES_RS).expect("write shapes.rs");
    let provider = make_provider(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "goto_definition",
            "arguments": { "symbol": "Point" }
        }),
    )
    .expect("goto_definition dispatch");

    insta::assert_json_snapshot!(response);
}

// ---------------------------------------------------------------------------
// Navigate: find_references
// ---------------------------------------------------------------------------

#[test]
fn golden_find_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("shapes.rs"), SHAPES_RS).expect("write shapes.rs");
    let provider = make_provider(dir.path());

    // Shape is defined (trait) and referenced (impl Shape for Point) in the
    // same file; include_definitions=true pulls both into the response.
    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "find_references",
            "arguments": {
                "symbol": "Shape",
                "include_definitions": true
            }
        }),
    )
    .expect("find_references dispatch");

    insta::assert_json_snapshot!(response);
}

// ---------------------------------------------------------------------------
// Navigate: call_hierarchy
// ---------------------------------------------------------------------------

#[test]
fn golden_call_hierarchy() {
    let dir = tempfile::tempdir().expect("tempdir");
    // lib.rs defines `helper`; main.rs has `compute` which calls `helper`.
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn helper() -> i32 {\n    42\n}\n",
    )
    .expect("write lib.rs");
    fs::write(
        dir.path().join("main.rs"),
        "pub fn compute(x: i32) -> i32 {\n    helper() * x\n}\n",
    )
    .expect("write main.rs");
    let provider = make_provider(dir.path());

    // direction="both": incoming should show `compute` (calls helper);
    // outgoing should be empty (helper has no function calls in its body).
    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "call_hierarchy",
            "arguments": {
                "symbol": "helper",
                "direction": "both"
            }
        }),
    )
    .expect("call_hierarchy dispatch");

    insta::assert_json_snapshot!(response);
}

// ---------------------------------------------------------------------------
// AST tools: ast_search
// ---------------------------------------------------------------------------

/// Tiny Rust file with two distinct call sites: foo and bar.
const SAMPLE_RS: &str = "\
fn main() {\n\
    foo(1);\n\
    bar(2);\n\
}\n\
\n\
fn foo(x: i32) {}\n\
fn bar(x: i32) {}\n";

#[test]
fn golden_ast_search() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("sample.rs"), SAMPLE_RS).expect("write sample.rs");
    let provider = make_provider(dir.path());

    // Query mode: tree-sitter S-expression capturing call-expression identifiers.
    let query_result = handle_tool_call(
        &provider,
        &json!({
            "name": "ast_search",
            "arguments": {
                "language": "rust",
                "query": "(call_expression function: (identifier) @name) @match",
                "max_results": 10
            }
        }),
    )
    .expect("ast_search query mode");

    // Pattern mode: $META variable matching a single-argument call to `foo`.
    let pattern_result = handle_tool_call(
        &provider,
        &json!({
            "name": "ast_search",
            "arguments": {
                "language": "rust",
                "pattern": "foo($ARG)",
                "max_results": 10
            }
        }),
    )
    .expect("ast_search pattern mode");

    insta::assert_json_snapshot!(json!({
        "query_mode": query_result,
        "pattern_mode": pattern_result,
    }));
}

// ---------------------------------------------------------------------------
// AST tools: ast_edit
// ---------------------------------------------------------------------------

#[test]
fn golden_ast_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("sample.rs"), SAMPLE_RS).expect("write sample.rs");
    let provider = make_provider(dir.path());

    // No-match: the pattern is absent → file is not modified, rewrites=0.
    let no_match = handle_tool_call(
        &provider,
        &json!({
            "name": "ast_edit",
            "arguments": {
                "path": "sample.rs",
                "pattern": "missing($ARG)",
                "replacement": "other(${ARG})"
            }
        }),
    )
    .expect("ast_edit no-match");

    // Pattern rewrite: replace foo($ARG) with baz(${ARG}) in sample.rs.
    // The file is modified in place; the response includes a unified diff.
    let edited = handle_tool_call(
        &provider,
        &json!({
            "name": "ast_edit",
            "arguments": {
                "path": "sample.rs",
                "pattern": "foo($ARG)",
                "replacement": "baz(${ARG})"
            }
        }),
    )
    .expect("ast_edit pattern rewrite");

    insta::assert_json_snapshot!(json!({
        "no_match": no_match,
        "pattern_edit": edited,
    }));
}
