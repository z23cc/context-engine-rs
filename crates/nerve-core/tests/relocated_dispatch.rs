//! Relocated provider-dependent dispatch tests from `nerve-core`'s in-src
//! `dispatch::tests` submodules (`args_text`, `editing`, `editing_selection`,
//! `tool_contracts`, `workspace`).
//!
//! These used to live in `nerve-core`'s in-src `#[cfg(test)]` modules but moved
//! out because they drive `nerve_fs::FsCatalogProvider` (the `dev-dependencies`
//! back-edge forbids constructing it in an in-src test, which would compile
//! `nerve-core` twice — "multiple versions of crate `nerve_core`"). They reach
//! only the public crate-root API plus `nerve_fs`, so they need no
//! `test_internals` re-export and run under a plain `cargo test` too. Every
//! assertion is byte-for-byte identical to the originals; only
//! `FsCatalogProvider`/`ScanOptions`/`FsWorkspaceRegistry` now come from
//! `nerve-fs`.

use nerve_core::*;
use nerve_fs::{FsCatalogProvider, FsWorkspaceRegistry, ScanOptions};
use serde_json::{Value, json};
use std::{fs, sync::Arc};

// ---- shared helpers (from dispatch/tests.rs) ----

fn provider_for(path: &std::path::Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![path.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    )
}

fn set_slice_selection(provider: &FsCatalogProvider, path: &str, start: usize, end: usize) {
    handle_tool_call(
        provider,
        &json!({ "name": "manage_selection", "arguments": {
            "op": "set", "mode": "slices",
            "slices": [{ "path": path, "ranges": [{ "start_line": start, "end_line": end }] }]
        } }),
    )
    .expect("set slice selection");
}

fn set_full_selection(provider: &FsCatalogProvider, path: &str) {
    handle_tool_call(
        provider,
        &json!({ "name": "manage_selection", "arguments": {
            "op": "set", "mode": "full", "paths": [path]
        } }),
    )
    .expect("set full selection");
}

fn selection_response(provider: &FsCatalogProvider) -> Value {
    handle_tool_call(
        provider,
        &json!({ "name": "manage_selection", "arguments": { "op": "get" } }),
    )
    .expect("selection get")
}

// ---- args_text.rs ----

#[test]
fn read_file_content_text_is_raw_not_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("default", Arc::new(provider_for(dir.path())));
    let response = handle_tool_call_with_resolver(
        &registry,
        &json!({ "name": "read_file", "arguments": { "path": "a.txt" } }),
    )
    .expect("read_file");
    assert_eq!(response["content"][0]["text"], json!("one\ntwo\nthree\n"));
    assert_eq!(
        response["structuredContent"]["content"],
        json!("one\ntwo\nthree\n")
    );
    assert_eq!(response["structuredContent"]["total_lines"], json!(3));
}

#[test]
fn hashline_read_file_structured_content_includes_rendered_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("write");
    let provider = provider_for(dir.path());
    let response = handle_tool_call(
        &provider,
        &json!({ "name": "read_file", "arguments": { "path": "a.txt", "view": "hashline" } }),
    )
    .expect("read_file hashline");
    assert_eq!(
        response["structuredContent"]["content"],
        response["content"][0]["text"]
    );
}

#[test]
fn file_tree_content_text_is_ascii_not_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "x\n").expect("write");
    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("default", Arc::new(provider_for(dir.path())));
    let response = handle_tool_call_with_resolver(
        &registry,
        &json!({ "name": "get_file_tree", "arguments": {} }),
    )
    .expect("get_file_tree");
    let text = response["content"][0]["text"].as_str().expect("text");
    assert!(
        !text.contains("\"children\""),
        "tree text must not be raw JSON"
    );
    assert!(text.contains("a.txt"));
    // structuredContent carries the compact ASCII `tree`, not a redundant
    // nested `roots` array (that would bloat the payload for clients).
    assert!(response["structuredContent"]["tree"].is_string());
    assert!(response["structuredContent"]["roots"].is_null());
}

#[test]
fn get_code_structure_reports_token_counts() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub struct Widget;\npub fn make_widget() -> Widget { Widget }\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());
    let response = handle_tool_call(
        &provider,
        &json!({ "name": "get_code_structure", "arguments": { "paths": ["lib.rs"] } }),
    )
    .expect("get_code_structure");
    let sc = &response["structuredContent"];
    let file_tokens = sc["files"][0]["token_count"].as_u64().expect("token_count");
    let total = sc["total_tokens"].as_u64().expect("total_tokens");
    assert!(file_tokens > 0, "per-file token_count should be positive");
    assert_eq!(
        total, file_tokens,
        "total_tokens == sum of file token_counts"
    );
}

#[test]
fn shebang_script_without_extension_has_code_structure() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("script"),
        "#!/usr/bin/env python3\nclass ScriptThing:\n    pass\n\ndef main():\n    return ScriptThing()\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());
    let response = handle_tool_call(
        &provider,
        &json!({ "name": "get_code_structure", "arguments": { "paths": ["script"] } }),
    )
    .expect("get_code_structure");
    assert_eq!(
        response["structuredContent"]["files"][0]["language"],
        json!("python")
    );
    let text = response["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("class ScriptThing"), "{text}");
    assert!(text.contains("def main()"), "{text}");
}

#[test]
fn markdown_fenced_code_has_code_structure() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("README.md"),
        "# Docs\n\n```rust\npub fn documented() {}\n```\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());
    let response = handle_tool_call(
        &provider,
        &json!({ "name": "get_code_structure", "arguments": { "paths": ["README.md"] } }),
    )
    .expect("get_code_structure");
    assert_eq!(
        response["structuredContent"]["files"][0]["language"],
        json!("markdown")
    );
    assert_eq!(
        response["structuredContent"]["files"][0]["symbols"][0]["line"],
        json!(4)
    );
    let text = response["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("pub fn documented()"), "{text}");
}

#[test]
fn shebang_script_summary_and_ast_search_work_without_extension() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("script"),
        "#!/usr/bin/env python3\ndef main():\n    return 1\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());
    let summary = handle_tool_call(
        &provider,
        &json!({ "name": "read_file", "arguments": { "path": "script", "view": "summary" } }),
    )
    .expect("summary");
    assert_eq!(summary["structuredContent"]["language"], json!("python"));
    assert_eq!(summary["structuredContent"]["parsed"], json!(true));

    let ast = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "python", "paths": ["script"],
            "query": "(function_definition name: (identifier) @name) @match" } }),
    )
    .expect("ast_search");
    assert_eq!(ast["structuredContent"]["files_scanned"], json!(1));
    assert_eq!(
        ast["structuredContent"]["matches"][0]["captures"]["name"],
        json!("main")
    );
}

#[test]
fn numeric_params_accept_stringified_ints() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("seed");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({ "name": "build_context", "arguments": {
            "query": "alpha", "token_budget": "200", "max_files": "5" } }),
    )
    .expect("build_context tolerates string numbers");

    handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "rust", "query": "(function_item) @match", "max_results": "10" } }),
    )
    .expect("ast_search tolerates string max_results");
}
// ---- editing.rs ----

#[test]
fn edit_tools_modify_filesystem_within_roots() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").expect("seed");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({ "name": "write", "arguments": { "path": "b.txt", "content": "hello\n" } }),
    )
    .expect("write");
    assert_eq!(
        fs::read_to_string(dir.path().join("b.txt")).expect("b.txt"),
        "hello\n"
    );

    handle_tool_call(
        &provider,
        &json!({ "name": "write", "arguments": { "path": "a/b/c.txt", "content": "nested\n" } }),
    )
    .expect("nested write");
    assert_eq!(
        fs::read_to_string(dir.path().join("a/b/c.txt")).expect("nested file"),
        "nested\n"
    );
    assert!(dir.path().join("a/b").is_dir());

    handle_tool_call(
        &provider,
        &json!({ "name": "edit", "arguments": { "mode": "replace", "path": "a.txt",
            "edits": [{ "old_text": "alpha", "new_text": "ALPHA" }] } }),
    )
    .expect("edit replace");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).expect("a.txt"),
        "ALPHA\nbeta\n"
    );

    let view = handle_tool_call(
        &provider,
        &json!({ "name": "read_file", "arguments": { "path": "a.txt", "view": "hashline" } }),
    )
    .expect("read hashline");
    let tag = view["structuredContent"]["hashline_tag"]
        .as_str()
        .expect("hashline_tag")
        .to_string();
    let patch = format!("*** Begin Patch\n[a.txt#{tag}]\nSWAP 2.=2:\n+BETA\n*** End Patch\n");
    handle_tool_call(
        &provider,
        &json!({ "name": "edit", "arguments": { "mode": "hashline", "patch": patch } }),
    )
    .expect("edit hashline");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).expect("a.txt"),
        "ALPHA\nBETA\n"
    );

    handle_tool_call(
        &provider,
        &json!({ "name": "move", "arguments": { "from": "b.txt", "to": "c.txt" } }),
    )
    .expect("move");
    assert!(!dir.path().join("b.txt").exists());
    assert_eq!(
        fs::read_to_string(dir.path().join("c.txt")).expect("c.txt"),
        "hello\n"
    );

    handle_tool_call(
        &provider,
        &json!({ "name": "delete", "arguments": { "path": "c.txt" } }),
    )
    .expect("delete");
    assert!(!dir.path().join("c.txt").exists());
}

#[test]
fn replace_symbol_body_updates_unique_symbol_definition() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() -> usize {\n    1\n}\n\npub fn beta() -> usize {\n    2\n}\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "replace_symbol_body",
            "arguments": {
                "symbol": "alpha",
                "path": "lib.rs",
                "body": "\npub fn alpha() -> usize {\n    42\n}\n"
            }
        }),
    )
    .expect("replace symbol");

    let content = fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs");
    assert_eq!(
        content,
        "pub fn alpha() -> usize {\n    42\n}\n\npub fn beta() -> usize {\n    2\n}\n"
    );
    assert_eq!(
        response["structuredContent"]["files"][0]["action"],
        "replace_symbol_body"
    );
    assert!(response["content"][0]["text"].as_str().is_some_and(|text| {
        text.contains("replace_symbol_body lib.rs")
            && text.contains("-    1")
            && text.contains("+    42")
    }));
}

#[test]
fn rename_symbol_updates_definition_and_same_file_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn helper() {}\n\npub fn caller() { helper(); helper(); }\n",
    )
    .expect("lib");
    fs::write(
        dir.path().join("other.rs"),
        "pub fn other() { helper(); }\n",
    )
    .expect("other");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper"
            }
        }),
    )
    .expect("rename symbol");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib"),
        "pub fn renamed_helper() {}\n\npub fn caller() { renamed_helper(); renamed_helper(); }\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("other.rs")).expect("other"),
        "pub fn other() { helper(); }\n"
    );
    assert!(response["content"][0]["text"].as_str().is_some_and(|text| {
        text.contains("rename_symbol lib.rs") && !text.contains("rename_symbol other.rs")
    }));
}

#[test]
fn rename_symbol_updates_import_backed_rust_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.rs"), "pub fn helper() {}\n").expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "use crate::target::helper;\n\npub fn caller() { helper(); }\n",
    )
    .expect("caller");
    fs::write(
        dir.path().join("other.rs"),
        "pub fn other() { helper(); }\n",
    )
    .expect("other");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.rs"
            }
        }),
    )
    .expect("import-backed rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("target.rs")).expect("target"),
        "pub fn renamed_helper() {}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "use crate::target::renamed_helper;\n\npub fn caller() { renamed_helper(); }\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("other.rs")).expect("other"),
        "pub fn other() { helper(); }\n"
    );
    let text = response["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("rename_symbol target.rs"));
    assert!(text.contains("rename_symbol caller.rs"));
    assert!(!text.contains("rename_symbol other.rs"));
}

#[test]
fn rename_symbol_updates_rust_grouped_import_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("target.rs"),
        "pub fn helper() {}\n\npub fn other() {}\n",
    )
    .expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "use crate::target::{helper, other};\n\npub fn caller() { helper(); other(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.rs"
            }
        }),
    )
    .expect("grouped import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "use crate::target::{renamed_helper, other};\n\npub fn caller() { renamed_helper(); other(); }\n"
    );
}

#[test]
fn rename_symbol_updates_rust_alias_import_specifier_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.rs"), "pub fn helper() {}\n").expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "use crate::target::helper as h;\n\npub fn caller(helper: fn()) { h(); helper(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.rs"
            }
        }),
    )
    .expect("rust alias import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "use crate::target::renamed_helper as h;\n\npub fn caller(helper: fn()) { h(); helper(); }\n"
    );
}

#[test]
fn rename_symbol_updates_rust_grouped_alias_import_specifier_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("target.rs"),
        "pub fn helper() {}\n\npub fn other() {}\n",
    )
    .expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "use crate::target::{helper as h, other};\n\npub fn caller() { h(); other(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.rs"
            }
        }),
    )
    .expect("grouped alias import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "use crate::target::{renamed_helper as h, other};\n\npub fn caller() { h(); other(); }\n"
    );
}

#[test]
fn rename_symbol_updates_python_alias_import_specifier_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("target.py"),
        "def helper():\n    return 1\n",
    )
    .expect("target");
    fs::write(
        dir.path().join("caller.py"),
        "from target import helper as h\n\ndef caller():\n    return h()\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.py"
            }
        }),
    )
    .expect("python alias import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("target.py")).expect("target"),
        "def renamed_helper():\n    return 1\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("caller.py")).expect("caller"),
        "from target import renamed_helper as h\n\ndef caller():\n    return h()\n"
    );
}

#[test]
fn rename_symbol_updates_javascript_alias_import_specifier_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("target.js"),
        "export function helper() { return 1; }\n",
    )
    .expect("target");
    fs::write(
        dir.path().join("caller.js"),
        "import { helper as h } from './target';\n\nexport function caller() { return h(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.js"
            }
        }),
    )
    .expect("javascript alias import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("target.js")).expect("target"),
        "export function renamed_helper() { return 1; }\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("caller.js")).expect("caller"),
        "import { renamed_helper as h } from './target';\n\nexport function caller() { return h(); }\n"
    );
}

#[test]
fn rename_symbol_noops_on_shadowed_importer() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.rs"), "pub fn helper() {}\n").expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "use crate::target::helper;\n\nfn helper() {}\n\npub fn caller() { helper(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.rs"
            }
        }),
    )
    .expect("shadowed importer rename");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["reason"], "shadowed_importer");
    assert_eq!(
        fs::read_to_string(dir.path().join("target.rs")).expect("target"),
        "pub fn helper() {}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "use crate::target::helper;\n\nfn helper() {}\n\npub fn caller() { helper(); }\n"
    );
}

#[test]
fn rename_symbol_ignores_commented_imports() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.rs"), "pub fn helper() {}\n").expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "// use crate::target::helper;\n/*\nuse crate::target::helper;\n*/\n\npub fn caller() { helper(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.rs"
            }
        }),
    )
    .expect("commented import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("target.rs")).expect("target"),
        "pub fn renamed_helper() {}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "// use crate::target::helper;\n/*\nuse crate::target::helper;\n*/\n\npub fn caller() { helper(); }\n"
    );
}

#[test]
fn rename_symbol_ignores_python_triple_quoted_imports() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("target.py"),
        "def helper():\n    return 1\n",
    )
    .expect("target");
    fs::write(
        dir.path().join("caller.py"),
        "\"\"\"\nfrom target import helper\n\"\"\"\n\ndef caller():\n    return helper()\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target.py"
            }
        }),
    )
    .expect("triple quoted import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("target.py")).expect("target"),
        "def renamed_helper():\n    return 1\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("caller.py")).expect("caller"),
        "\"\"\"\nfrom target import helper\n\"\"\"\n\ndef caller():\n    return helper()\n"
    );
}

#[test]
fn rename_symbol_updates_unicode_prefixed_import_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target_π.rs"), "pub fn helper() {}\n").expect("target");
    fs::write(
        dir.path().join("caller.rs"),
        "use crate::target_π::helper;\n\npub fn caller() { helper(); }\n",
    )
    .expect("caller");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "target_π.rs"
            }
        }),
    )
    .expect("unicode import rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("caller.rs")).expect("caller"),
        "use crate::target_π::renamed_helper;\n\npub fn caller() { renamed_helper(); }\n"
    );
}

#[test]
fn rename_symbol_ambiguous_definition_does_not_mutate() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "pub fn helper() {}\n").expect("a");
    fs::write(dir.path().join("b.rs"), "pub fn helper() {}\n").expect("b");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": { "symbol": "helper", "new_name": "renamed_helper" }
        }),
    )
    .expect("ambiguous rename");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["reason"], "ambiguous_symbol");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).expect("a"),
        "pub fn helper() {}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.rs")).expect("b"),
        "pub fn helper() {}\n"
    );
}

#[test]
fn rename_symbol_rejects_invalid_new_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn helper() {}\n").expect("lib");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": { "symbol": "helper", "new_name": "bad-name" }
        }),
    )
    .expect("invalid rename");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["reason"], "invalid_new_name");
    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib"),
        "pub fn helper() {}\n"
    );
}

#[test]
fn rename_symbol_rejects_keyword_new_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn helper() {}\n").expect("lib");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": { "symbol": "helper", "new_name": "fn" }
        }),
    )
    .expect("keyword rename");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["reason"], "invalid_new_name");
    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib"),
        "pub fn helper() {}\n"
    );
}

#[test]
fn rename_symbol_same_name_is_noop() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn helper() {}\n").expect("lib");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": { "symbol": "helper", "new_name": "helper" }
        }),
    )
    .expect("same-name rename");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["reason"], "no_op");
    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib"),
        "pub fn helper() {}\n"
    );
}

#[test]
fn rename_symbol_uses_byte_columns_after_unicode_prefix() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn helper() {}\n\npub fn caller() { let π = 1; helper(); helper(); }\n",
    )
    .expect("lib");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": { "symbol": "helper", "new_name": "renamed_helper" }
        }),
    )
    .expect("unicode-prefix rename");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib"),
        "pub fn renamed_helper() {}\n\npub fn caller() { let π = 1; renamed_helper(); renamed_helper(); }\n"
    );
}

#[test]
fn rename_symbol_targets_root_scoped_match_in_multi_root_provider() {
    let left = tempfile::tempdir().expect("left");
    let right = tempfile::tempdir().expect("right");
    fs::write(
        left.path().join("lib.rs"),
        "pub fn helper() {}\n\npub fn caller() { helper(); }\n",
    )
    .expect("left");
    fs::write(
        right.path().join("lib.rs"),
        "pub fn helper() {}\n\npub fn caller() { helper(); }\n",
    )
    .expect("right");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![left.path().to_path_buf(), right.path().to_path_buf()])
            .expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "rename_symbol",
            "arguments": {
                "symbol": "helper",
                "new_name": "renamed_helper",
                "path": "root-1/lib.rs"
            }
        }),
    )
    .expect("root-scoped rename");

    assert_eq!(
        fs::read_to_string(left.path().join("lib.rs")).expect("left"),
        "pub fn helper() {}\n\npub fn caller() { helper(); }\n"
    );
    assert_eq!(
        fs::read_to_string(right.path().join("lib.rs")).expect("right"),
        "pub fn renamed_helper() {}\n\npub fn caller() { renamed_helper(); }\n"
    );
    assert_eq!(
        response["structuredContent"]["files"][0]["path"],
        "root-1/lib.rs"
    );
}

#[test]
fn replace_symbol_body_preserves_neighbor_without_trailing_newline() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    old();\n}\n\npub fn beta() {\n    beta();\n}",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "replace_symbol_body",
            "arguments": {
                "symbol": "alpha",
                "path": "lib.rs",
                "body": "pub fn alpha() {\n    new();\n}"
            }
        }),
    )
    .expect("replace symbol");

    let content = fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs");
    assert_eq!(
        content,
        "pub fn alpha() {\n    new();\n}\n\npub fn beta() {\n    beta();\n}"
    );
    assert_eq!(content.matches("pub fn beta()").count(), 1);
}

#[test]
fn replace_symbol_body_ambiguous_symbol_does_not_mutate() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "pub fn alpha() {\n    a();\n}\n").expect("a");
    fs::write(dir.path().join("b.rs"), "pub fn alpha() {\n    b();\n}\n").expect("b");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "replace_symbol_body",
            "arguments": {
                "symbol": "alpha",
                "body": "pub fn alpha() { changed(); }"
            }
        }),
    )
    .expect("ambiguous replace");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["total"], Value::from(2));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).expect("a"),
        "pub fn alpha() {\n    a();\n}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.rs")).expect("b"),
        "pub fn alpha() {\n    b();\n}\n"
    );
}

#[test]
fn insert_before_symbol_inserts_before_unique_symbol() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn beta() {\n    beta();\n}\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "insert_before_symbol",
            "arguments": {
                "symbol": "beta",
                "path": "lib.rs",
                "body": "pub fn alpha() {\n    alpha();\n}\n\n"
            }
        }),
    )
    .expect("insert before symbol");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}\n\npub fn beta() {\n    beta();\n}\n"
    );
    assert_eq!(
        response["structuredContent"]["files"][0]["action"],
        "insert_before_symbol"
    );
}

#[test]
fn insert_after_symbol_inserts_after_unique_symbol() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}\n\npub fn gamma() {\n    gamma();\n}\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "insert_after_symbol",
            "arguments": {
                "symbol": "alpha",
                "path": "lib.rs",
                "body": "pub fn beta() {\n    beta();\n}\n"
            }
        }),
    )
    .expect("insert after symbol");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}\npub fn beta() {\n    beta();\n}\n\npub fn gamma() {\n    gamma();\n}\n"
    );
    assert_eq!(
        response["structuredContent"]["files"][0]["action"],
        "insert_after_symbol"
    );
}

#[test]
fn insert_after_symbol_handles_symbol_at_eof_without_trailing_newline() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "insert_after_symbol",
            "arguments": {
                "symbol": "alpha",
                "path": "lib.rs",
                "body": "pub fn beta() {\n    beta();\n}"
            }
        }),
    )
    .expect("insert after eof symbol");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}\npub fn beta() {\n    beta();\n}\n"
    );
}

#[test]
fn insert_after_symbol_preserves_explicit_leading_newline_at_eof() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "insert_after_symbol",
            "arguments": {
                "symbol": "alpha",
                "path": "lib.rs",
                "body": "\npub fn beta() {\n    beta();\n}"
            }
        }),
    )
    .expect("insert after eof symbol");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}\npub fn beta() {\n    beta();\n}\n"
    );
}

#[test]
fn insert_after_symbol_empty_body_at_eof_preserves_content() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    handle_tool_call(
        &provider,
        &json!({
            "name": "insert_after_symbol",
            "arguments": {
                "symbol": "alpha",
                "path": "lib.rs",
                "body": ""
            }
        }),
    )
    .expect("empty insert after eof symbol");

    assert_eq!(
        fs::read_to_string(dir.path().join("lib.rs")).expect("lib.rs"),
        "pub fn alpha() {\n    alpha();\n}"
    );
}

#[test]
fn insert_after_symbol_targets_root_scoped_match_in_multi_root_provider() {
    let left = tempfile::tempdir().expect("left");
    let right = tempfile::tempdir().expect("right");
    fs::write(
        left.path().join("lib.rs"),
        "pub fn alpha() {\n    same();\n}\n",
    )
    .expect("left");
    fs::write(
        right.path().join("lib.rs"),
        "pub fn alpha() {\n    same();\n}\n",
    )
    .expect("right");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![left.path().to_path_buf(), right.path().to_path_buf()])
            .expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "insert_after_symbol",
            "arguments": {
                "symbol": "alpha",
                "path": "root-1/lib.rs",
                "body": "pub fn beta() {\n    beta();\n}\n"
            }
        }),
    )
    .expect("insert after right root symbol");

    assert_eq!(
        fs::read_to_string(left.path().join("lib.rs")).expect("left lib"),
        "pub fn alpha() {\n    same();\n}\n"
    );
    assert_eq!(
        fs::read_to_string(right.path().join("lib.rs")).expect("right lib"),
        "pub fn alpha() {\n    same();\n}\npub fn beta() {\n    beta();\n}\n"
    );
    assert_eq!(
        response["structuredContent"]["files"][0]["path"],
        "root-1/lib.rs"
    );
}

#[test]
fn insert_after_symbol_ambiguous_symbol_does_not_mutate() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "pub fn alpha() {\n    a();\n}\n").expect("a");
    fs::write(dir.path().join("b.rs"), "pub fn alpha() {\n    b();\n}\n").expect("b");
    let provider = provider_for(dir.path());

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "insert_after_symbol",
            "arguments": {
                "symbol": "alpha",
                "body": "pub fn beta() { beta(); }"
            }
        }),
    )
    .expect("ambiguous insert");

    assert_eq!(response["structuredContent"]["mutated"], Value::Bool(false));
    assert_eq!(response["structuredContent"]["total"], Value::from(2));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).expect("a"),
        "pub fn alpha() {\n    a();\n}\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("b.rs")).expect("b"),
        "pub fn alpha() {\n    b();\n}\n"
    );
}

#[test]
fn fs_provider_create_does_not_overwrite_existing_in_batch_api() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("exists.txt"), "old\n").expect("seed exists");
    let provider = provider_for(dir.path());
    let changes = [edit::FileChange::Create {
        path: "exists.txt".to_string(),
        content: "new\n".to_string(),
    }];
    assert!(provider.apply_file_batch(&changes, false).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("exists.txt")).unwrap(),
        "old\n"
    );
    assert!(provider.apply_file_batch(&changes, true).is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("exists.txt")).unwrap(),
        "old\n"
    );
}
// ---- editing_selection.rs ----

#[test]
fn stale_hash_error_has_structured_reread_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "current\n").expect("seed");
    let provider = provider_for(dir.path());
    let patch = "*** Begin Patch\n[a.txt#0000000000000000]\nSWAP 1.=1:\n+x\n*** End Patch\n";
    let err = handle_tool_call(
        &provider,
        &json!({ "name": "edit", "arguments": { "mode": "hashline", "patch": patch } }),
    )
    .expect_err("stale hash");
    let value = dispatch_error_value(&err);
    assert_eq!(value["error"]["kind"], json!("stale_hash"));
    assert_eq!(value["error"]["path"], json!("a.txt"));
    assert_eq!(value["error"]["expected_hash"], json!("0000000000000000"));
    assert!(value["error"]["actual_hash"].is_string());
    assert!(
        value["error"]["reread_hint"]
            .as_str()
            .unwrap()
            .contains("hashline")
    );
    assert!(value["error"].get("content").is_none());
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "current\n"
    );
}

#[test]
fn selection_slices_rebase_across_mutation_tools() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").expect("seed a");
    fs::write(dir.path().join("w.txt"), "alpha\nbeta\ngamma\n").expect("seed w");
    fs::write(
        dir.path().join("a.rs"),
        "fn main() {\n    foo();\n    selected();\n}\n",
    )
    .expect("seed rs");
    fs::write(dir.path().join("m.txt"), "move me\n").expect("seed m");
    let provider = provider_for(dir.path());

    set_slice_selection(&provider, "a.txt", 3, 3);
    let edit = handle_tool_call(
        &provider,
        &json!({ "name": "edit", "arguments": { "mode": "replace", "path": "a.txt",
            "edits": [{ "old_text": "one\n", "new_text": "zero\none\n" }] } }),
    )
    .expect("edit replace");
    assert_eq!(
        edit["structuredContent"]["files"][0]["selection"]["ranges_after"][0]["start_line"],
        json!(4)
    );

    set_slice_selection(&provider, "w.txt", 2, 2);
    let write = handle_tool_call(
        &provider,
        &json!({ "name": "write", "arguments": { "path": "w.txt", "content": "replacement\n" } }),
    )
    .expect("write");
    assert_eq!(
        write["structuredContent"]["files"][0]["selection"]["dropped"][0]["start_line"],
        json!(2)
    );
    let selection = selection_response(&provider);
    assert!(
        selection["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    set_slice_selection(&provider, "a.rs", 3, 3);
    handle_tool_call(
        &provider,
        &json!({ "name": "ast_edit", "arguments": {
            "path": "a.rs", "mode": "pattern", "pattern": "foo()",
            "replacement": "foo();\n    inserted()" } }),
    )
    .expect("ast edit");
    let selection = selection_response(&provider);
    assert_eq!(
        selection["structuredContent"]["files"][0]["ranges"][0]["start_line"],
        json!(4)
    );

    set_full_selection(&provider, "m.txt");
    handle_tool_call(
        &provider,
        &json!({ "name": "move", "arguments": { "from": "m.txt", "to": "moved.txt" } }),
    )
    .expect("move selected");
    let moved = selection_response(&provider);
    assert_eq!(
        moved["structuredContent"]["files"][0]["path"],
        json!("moved.txt")
    );

    handle_tool_call(
        &provider,
        &json!({ "name": "delete", "arguments": { "path": "moved.txt" } }),
    )
    .expect("delete selected");
    let deleted = selection_response(&provider);
    assert!(
        deleted["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn move_over_selected_destination_drops_stale_destination_slice() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("src.txt"), "new\ncontent\n").expect("seed src");
    fs::write(dir.path().join("dst.txt"), "old\nselected\n").expect("seed dst");
    let provider = provider_for(dir.path());
    set_slice_selection(&provider, "dst.txt", 2, 2);

    let moved = handle_tool_call(
        &provider,
        &json!({ "name": "move", "arguments": { "from": "src.txt", "to": "dst.txt" } }),
    )
    .expect("move over selected destination");
    assert_eq!(
        moved["structuredContent"]["files"][0]["selection"]["dropped"][0]["start_line"],
        json!(2)
    );
    let selection = selection_response(&provider);
    assert!(
        selection["structuredContent"]["files"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn write_outside_roots_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = provider_for(dir.path());
    let result = handle_tool_call(
        &provider,
        &json!({ "name": "write", "arguments": { "path": "../escape.txt", "content": "x" } }),
    );
    assert!(result.is_err(), "writes outside roots must be rejected");
}

#[test]
fn edit_reports_syntax_diagnostics_on_broken_rust() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn main() {\n    let x = 1;\n}\n").expect("seed");
    let provider = provider_for(dir.path());
    // Drop the closing brace to break the syntax.
    let result = handle_tool_call(
        &provider,
        &json!({ "name": "edit", "arguments": { "mode": "replace", "path": "a.rs",
            "edits": [{ "old_text": "}\n", "new_text": "\n" }] } }),
    )
    .expect("edit");
    let diagnostics = result["structuredContent"]["files"][0]["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !diagnostics.is_empty(),
        "expected syntax diagnostics for broken Rust"
    );
}

#[test]
fn write_reports_syntax_diagnostics_for_markdown_fenced_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let provider = provider_for(dir.path());
    let content = "# Notes\n\n```rust\npub fn broken() {\n    let = 1;\n}\n```\n";

    let result = handle_tool_call(
        &provider,
        &json!({ "name": "write", "arguments": { "path": "README.md", "content": content } }),
    )
    .expect("write");

    let diagnostics = result["structuredContent"]["files"][0]["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        diagnostics.iter().any(|issue| issue["line"] == json!(5)
            && issue["message"]
                .as_str()
                .is_some_and(|message| message.starts_with("rust fenced code: "))),
        "diagnostics: {diagnostics:?}"
    );
    assert!(
        result["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("README.md line 5: rust fenced code:"))
    );
}

#[test]
fn ast_search_and_edit_tools() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn main() { foo(); bar(); }\n").expect("seed");
    let provider = provider_for(dir.path());

    let res = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "rust",
            "query": "(call_expression function: (identifier) @name) @match" } }),
    )
    .expect("ast_search");
    assert_eq!(
        res["structuredContent"]["matches"]
            .as_array()
            .map(|matches| matches.len()),
        Some(2)
    );

    handle_tool_call(
        &provider,
        &json!({ "name": "ast_edit", "arguments": {
            "path": "a.rs",
            "query": "(call_expression) @match",
            "replacement": "done()" } }),
    )
    .expect("ast_edit");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).expect("read"),
        "fn main() { done(); done(); }\n"
    );
}

#[test]
fn ast_search_finds_markdown_fenced_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("README.md"),
        "# Example\n\n```rust\npub fn fenced() {\n    foo();\n}\n```\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let res = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "rust",
            "query": "(function_item name: (identifier) @name) @match" } }),
    )
    .expect("ast_search");

    assert_eq!(res["structuredContent"]["files_scanned"], json!(1));
    assert_eq!(res["structuredContent"]["matches"][0]["path"], "README.md");
    assert_eq!(res["structuredContent"]["matches"][0]["line"], json!(4));
    assert_eq!(
        res["structuredContent"]["matches"][0]["captures"]["name"],
        "fenced"
    );
}

#[test]
fn ast_search_deindents_indented_markdown_fences() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("README.md"),
        "   ```python\n   def accepted():\n       return 1\n   ```\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let res = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "python",
            "query": "(function_definition name: (identifier) @name) @match" } }),
    )
    .expect("ast_search");

    assert_eq!(res["structuredContent"]["files_scanned"], json!(1));
    assert_eq!(res["structuredContent"]["matches"][0]["line"], json!(2));
    assert_eq!(
        res["structuredContent"]["matches"][0]["captures"]["name"],
        "accepted"
    );
}

#[test]
fn ast_search_ignores_supported_markers_inside_unsupported_fences() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("README.md"),
        "```text\n```rust\npub fn ignored() {}\n```\n```\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let res = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "rust",
            "query": "(function_item name: (identifier) @name) @match" } }),
    )
    .expect("ast_search");

    assert_eq!(res["structuredContent"]["files_scanned"], json!(0));
    assert_eq!(
        res["structuredContent"]["matches"]
            .as_array()
            .map(|matches| matches.len()),
        Some(0)
    );
}

#[test]
fn ast_search_scoped_directory_skips_unsupported_non_markdown_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("docs")).expect("docs");
    fs::write(
        dir.path().join("docs").join("notes.txt"),
        "```rust\npub fn should_not_scan() {}\n```\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let res = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "rust",
            "paths": ["docs"],
            "query": "(function_item name: (identifier) @name) @match" } }),
    )
    .expect("ast_search");

    assert_eq!(res["structuredContent"]["files_scanned"], json!(0));
    assert_eq!(
        res["structuredContent"]["matches"]
            .as_array()
            .map(|matches| matches.len()),
        Some(0)
    );
}

#[test]
fn ast_pattern_search_and_edit_tools() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("a.rs"),
        "fn main() { foo(one); bar(one); }\n",
    )
    .expect("seed");
    let provider = provider_for(dir.path());

    let res = handle_tool_call(
        &provider,
        &json!({ "name": "ast_search", "arguments": {
            "language": "rust",
            "mode": "pattern",
            "pattern": "foo($ARG)" } }),
    )
    .expect("ast_search");
    assert_eq!(
        res["structuredContent"]["matches"]
            .as_array()
            .map(|matches| matches.len()),
        Some(1)
    );
    assert_eq!(
        res["structuredContent"]["matches"][0]["captures"]["ARG"],
        "one"
    );

    handle_tool_call(
        &provider,
        &json!({ "name": "ast_edit", "arguments": {
            "path": "a.rs",
            "mode": "pattern",
            "pattern": "foo($ARG)",
            "replacement": "baz(${ARG})" } }),
    )
    .expect("ast_edit");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.rs")).expect("read"),
        "fn main() { baz(one); bar(one); }\n"
    );
}

#[test]
fn git_tool_and_per_edit_diff() {
    if git_missing() {
        return;
    }
    let dir = git_fixture();
    let provider = provider_for(dir.path());

    assert_clean_git_diff_bundle(&provider);
    assert_edit_diff_and_raw_git_diff(&provider);
    assert_legacy_git_structured_output(&provider);
    fs::write(dir.path().join("b file.txt"), "ALPHA\nBETA\nGAMMA\nDELTA\n").expect("edit b");
    assert_churn_sorted_git_diff_modes(&provider);
    git_run(dir.path(), &["add", "a.txt"]);
    assert_staged_git_diff(&provider);
    assert_git_status_lists_file(&provider);
}

fn git_missing() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_err()
}

fn git_fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git_run(dir.path(), &["init", "-q"]);
    fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("seed");
    fs::write(dir.path().join("b file.txt"), "alpha\nbeta\ngamma\ndelta\n").expect("seed b");
    git_run(dir.path(), &["add", "."]);
    git_run(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn git_run(root: &std::path::Path, args: &[&str]) {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git");
}

fn assert_edit_diff_and_raw_git_diff(provider: &FsCatalogProvider) {
    let res = handle_tool_call(
        provider,
        &json!({ "name": "edit", "arguments": { "mode": "replace", "path": "a.txt",
            "edits": [{ "old_text": "two", "new_text": "TWO" }] } }),
    )
    .expect("edit");
    let diff = res["structuredContent"]["files"][0]["diff"]
        .as_str()
        .unwrap_or("");
    assert!(
        diff.contains("-two") && diff.contains("+TWO"),
        "diff: {diff}"
    );

    let g = handle_tool_call(
        provider,
        &json!({ "name": "git", "arguments": { "op": "diff" } }),
    )
    .expect("git diff");
    assert!(
        g["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("+TWO")
    );
}

fn assert_clean_git_diff_bundle(provider: &FsCatalogProvider) {
    let bundle = git_response(provider, json!({ "op": "diff", "detail": "bundle" }));
    assert!(
        bundle["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("(no changes)")
    );
    assert_eq!(bundle["structuredContent"]["detail"], json!("bundle"));
    assert_eq!(
        bundle["structuredContent"]["files"]
            .as_array()
            .expect("files")
            .len(),
        0
    );
    assert_eq!(bundle["structuredContent"]["truncated"], json!(false));
}

fn assert_churn_sorted_git_diff_modes(provider: &FsCatalogProvider) {
    let files_text = git_text(provider, json!({ "op": "diff", "detail": "files" }));
    assert!(files_text.contains("b file.txt (+4 -4)"), "{files_text}");
    assert!(files_text.contains("a.txt (+1 -1)"), "{files_text}");
    assert!(files_text.find("b file.txt").unwrap() < files_text.find("a.txt").unwrap());

    let filtered_text = git_text(
        provider,
        json!({ "op": "diff", "detail": "files", "path": "a.txt" }),
    );
    assert!(filtered_text.contains("a.txt (+1 -1)"), "{filtered_text}");
    assert!(!filtered_text.contains("b file.txt"), "{filtered_text}");

    let zero_budget = handle_tool_call(
        provider,
        &json!({ "name": "git", "arguments": { "op": "diff", "detail": "patches", "max_chars": 0 } }),
    );
    assert!(zero_budget.is_err(), "max_chars=0 should be rejected");

    let patch_text = git_text(
        provider,
        json!({ "op": "diff", "detail": "patches", "max_chars": 4000 }),
    );
    assert!(
        patch_text.contains("# file: b file.txt (+4 -4)"),
        "{patch_text}"
    );
    assert!(patch_text.contains("# file: a.txt (+1 -1)"), "{patch_text}");
    assert!(
        patch_text.find("# file: b file.txt").unwrap() < patch_text.find("# file: a.txt").unwrap()
    );

    let bundle = git_response(
        provider,
        json!({ "op": "diff", "detail": "bundle", "max_chars": 4000 }),
    );
    assert_eq!(bundle["structuredContent"]["detail"], json!("bundle"));
    assert_eq!(
        bundle["structuredContent"]["files"][0]["path"],
        json!("b file.txt")
    );
    assert_eq!(bundle["structuredContent"]["files"][0]["churn"], json!(8));
    assert_eq!(
        bundle["structuredContent"]["files"][1]["path"],
        json!("a.txt")
    );
    assert_eq!(
        bundle["structuredContent"]["included_patch_count"],
        json!(2)
    );
    assert_eq!(bundle["structuredContent"]["omitted_patch_count"], json!(0));
    assert_eq!(bundle["structuredContent"]["truncated"], json!(false));
    let full_payload_chars = bundle_patch_payload_chars(&bundle);
    let exact = git_response(
        provider,
        json!({ "op": "diff", "detail": "bundle", "max_chars": full_payload_chars }),
    );
    assert_eq!(exact["structuredContent"]["truncated"], json!(false));
    assert_eq!(exact["structuredContent"]["included_patch_count"], json!(2));
    assert_eq!(bundle_patch_payload_chars(&exact), full_payload_chars);
    let first_patch = bundle["structuredContent"]["patches"][0]["patch"]
        .as_str()
        .expect("patch");
    assert!(first_patch.contains("+ALPHA"), "{first_patch}");
    assert!(
        bundle["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .contains("patches: included 2/2")
    );

    let truncated = git_response(
        provider,
        json!({ "op": "diff", "detail": "bundle", "max_chars": 12 }),
    );
    assert_eq!(truncated["structuredContent"]["truncated"], json!(true));
    assert_eq!(
        truncated["structuredContent"]["files"]
            .as_array()
            .expect("files")
            .len(),
        2
    );
    assert!(bundle_patch_payload_chars(&truncated) <= 12);
    assert!(
        truncated["structuredContent"]["truncated_patch_count"]
            .as_u64()
            .unwrap()
            > 0
            || truncated["structuredContent"]["omitted_patch_count"]
                .as_u64()
                .unwrap()
                > 0
    );
    assert!(truncated["structuredContent"]["truncation"].is_object());
}

fn assert_legacy_git_structured_output(provider: &FsCatalogProvider) {
    for arguments in [
        json!({ "op": "diff" }),
        json!({ "op": "diff", "detail": "summary" }),
        json!({ "op": "diff", "detail": "files" }),
        json!({ "op": "diff", "detail": "patches", "max_chars": 4000 }),
        json!({ "op": "status" }),
    ] {
        let response = git_response(provider, arguments);
        assert_eq!(
            response["structuredContent"]["output"], response["content"][0]["text"],
            "legacy git modes should keep structuredContent.output"
        );
    }
}

fn bundle_patch_payload_chars(response: &Value) -> usize {
    response["structuredContent"]["patches"]
        .as_array()
        .expect("patches")
        .iter()
        .map(|patch| patch["patch"].as_str().unwrap_or("").chars().count())
        .sum()
}

fn assert_staged_git_diff(provider: &FsCatalogProvider) {
    let staged_text = git_text(
        provider,
        json!({ "op": "diff", "detail": "files", "staged": true }),
    );
    assert!(staged_text.contains("a.txt (+1 -1)"), "{staged_text}");
    assert!(!staged_text.contains("b file.txt"), "{staged_text}");
}

fn assert_git_status_lists_file(provider: &FsCatalogProvider) {
    let status = git_text(provider, json!({ "op": "status" }));
    assert!(status.contains("a.txt"));
}

fn git_text(provider: &FsCatalogProvider, arguments: Value) -> String {
    let response = git_response(provider, arguments);
    response["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

fn git_response(provider: &FsCatalogProvider, arguments: Value) -> Value {
    handle_tool_call(provider, &json!({ "name": "git", "arguments": arguments })).expect("git call")
}

// ---- tool_contracts.rs ----

#[test]
fn cancellable_json_dispatch_returns_cancelled_error_object() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("text.txt"), "needle\n").expect("write");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );
    let token = CancelToken::new();
    token.cancel();

    let json = handle_tool_call_json_cancellable(
        &provider,
        r#"{"name":"file_search","arguments":{"pattern":"needle","mode":"content"}}"#,
        &token,
    )
    .expect("cancelled dispatch is encoded as JSON");
    let value: Value = serde_json::from_str(&json).expect("json");
    assert_eq!(value["error"]["kind"], "cancelled");
}

#[test]
fn tool_search_is_listed_and_searches_catalog_without_workspace() {
    let left = tempfile::tempdir().expect("left");
    let right = tempfile::tempdir().expect("right");
    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("left", Arc::new(provider_for(left.path())));
    registry.insert("right", Arc::new(provider_for(right.path())));

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"tool_search"));

    let response = handle_tool_call_with_resolver(
        &registry,
        &json!({ "name": "tool_search", "arguments": { "query": "git diff patch", "max_results": "3" } }),
    )
    .expect("tool search dispatch");

    assert_eq!(response["structuredContent"]["matches"][0]["name"], "git");
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("git (score"))
    );
    assert_eq!(
        response["structuredContent"]["matches"]
            .as_array()
            .expect("matches")
            .len(),
        3
    );

    let zero = handle_tool_call_with_resolver(
        &registry,
        &json!({ "name": "tool_search", "arguments": { "query": "git diff", "max_results": 0 } }),
    )
    .expect("zero-result tool search");
    assert_eq!(
        zero["structuredContent"]["matches"]
            .as_array()
            .expect("matches")
            .len(),
        0
    );
    assert!(
        zero["structuredContent"]["matched_tools"]
            .as_u64()
            .unwrap_or(0)
            > 0
    );
}

#[test]
fn symbol_search_is_listed_and_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("payments.rs"),
        "pub struct PaymentGateway;\npub fn process_payment() {}\n",
    )
    .expect("write payments");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"symbol_search"));

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "symbol_search",
            "arguments": { "query": "pay gate", "max_results": "1" }
        }),
    )
    .expect("symbol search dispatch");

    assert_eq!(
        response["structuredContent"]["matches"][0]["name"],
        Value::String("PaymentGateway".to_string())
    );
    assert_eq!(
        response["structuredContent"]["matches"]
            .as_array()
            .expect("matches")
            .len(),
        1
    );
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("PaymentGateway"))
    );

    let zero = handle_tool_call(
        &provider,
        &json!({
            "name": "symbol_search",
            "arguments": { "query": "payment", "max_results": 0 }
        }),
    )
    .expect("zero-result symbol search");
    assert_eq!(
        zero["structuredContent"]["matches"]
            .as_array()
            .expect("matches")
            .len(),
        0
    );
    assert_eq!(zero["structuredContent"]["total"].as_u64(), Some(2));
    assert!(
        zero["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("2 matches") && text.contains("showing 0"))
    );
}

#[test]
fn find_referencing_symbols_is_listed_and_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("helper.rs"), "pub fn helper() {}\n").expect("helper");
    fs::write(
        dir.path().join("caller.rs"),
        "pub fn caller() {\n    helper();\n}\n",
    )
    .expect("caller");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"find_referencing_symbols"));

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "find_referencing_symbols",
            "arguments": { "symbol": "helper", "max_results": "10", "context_lines": "1" }
        }),
    )
    .expect("referencing-symbols dispatch");

    let referencing = response["structuredContent"]["referencing_symbols"]
        .as_array()
        .expect("referencing symbols");
    assert_eq!(referencing.len(), 1);
    assert_eq!(referencing[0]["symbol"], "caller");
    assert_eq!(referencing[0]["column"], Value::from(8));
    assert_eq!(referencing[0]["reference_line"], Value::from(2));
    assert_eq!(referencing[0]["reference_column"], Value::from(5));
    assert!(
        referencing[0]["reference_context"]
            .as_str()
            .is_some_and(|text| text.contains("2:     helper();"))
    );
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(
                |text| text.contains("find_referencing_symbols") && text.contains("caller")
            )
    );
}

#[test]
fn analyze_impact_is_listed_and_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("helper.rs"), "pub fn helper() {}\n").expect("helper");
    fs::write(
        dir.path().join("middle.rs"),
        "pub fn middle() { helper(); }\n",
    )
    .expect("middle");
    fs::write(dir.path().join("top.rs"), "pub fn top() { middle(); }\n").expect("top");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"analyze_impact"));

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "analyze_impact",
            "arguments": { "symbol": "helper", "max_depth": "2", "max_results": "10" }
        }),
    )
    .expect("impact dispatch");

    assert_eq!(
        response["structuredContent"]["definitions"][0]["path"],
        "helper.rs"
    );
    assert_eq!(
        response["structuredContent"]["definitions"][0]["column"],
        Value::from(8)
    );
    let impacted = response["structuredContent"]["impacted"]
        .as_array()
        .expect("impacted");
    assert!(impacted.iter().any(|item| {
        item["symbol"] == "middle"
            && item["depth"] == 1
            && item["column"] == 8
            && item["reference_column"] == 19
    }));
    assert!(
        impacted
            .iter()
            .any(|item| item["symbol"] == "top" && item["depth"] == 2)
    );
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("analyze_impact") && text.contains("d1"))
    );
}

#[test]
fn detect_changes_is_listed_and_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    let x = 1;\n}\npub fn beta() {}\n",
    )
    .expect("lib");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"detect_changes"));

    let diff = "--- a/lib.rs\n+++ b/lib.rs\n@@ -1,3 +1,3 @@\n pub fn alpha() {\n-    let x = 1;\n+    let x = 42;\n }\n";
    let response = handle_tool_call(
        &provider,
        &json!({ "name": "detect_changes", "arguments": { "diff": diff } }),
    )
    .expect("detect_changes dispatch");

    let files = response["structuredContent"]["files"]
        .as_array()
        .expect("files");
    assert!(files.iter().any(|file| {
        file["display_path"] == "lib.rs"
            && file["affected"]
                .as_array()
                .is_some_and(|symbols| symbols.iter().any(|symbol| symbol["name"] == "alpha"))
    }));
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("detect_changes") && text.contains("alpha"))
    );
}

#[test]
fn trace_path_is_listed_and_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("lib.rs"),
        "pub fn top() { middle(); }\npub fn middle() { leaf(); }\npub fn leaf() {}\n",
    )
    .expect("lib");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"trace_path"));

    let response = handle_tool_call(
        &provider,
        &json!({ "name": "trace_path", "arguments": { "from": "top", "to": "leaf" } }),
    )
    .expect("trace_path dispatch");

    assert_eq!(response["structuredContent"]["found"], true);
    let path: Vec<_> = response["structuredContent"]["path"]
        .as_array()
        .expect("path")
        .iter()
        .filter_map(|step| step["symbol"].as_str())
        .collect();
    assert_eq!(path, vec!["top", "middle", "leaf"]);
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("trace_path"))
    );
}

#[test]
fn read_symbol_is_listed_and_dispatches_body_or_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("a.rs"),
        "pub fn alpha() -> usize {\n    1\n}\n",
    )
    .expect("write a");
    fs::write(dir.path().join("b.rs"), "pub fn beta() {}\n").expect("write b");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"read_symbol"));

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "read_symbol",
            "arguments": { "symbol": "alpha", "max_matches": "1" }
        }),
    )
    .expect("read_symbol dispatch");
    assert_eq!(response["structuredContent"]["total"], Value::from(1));
    assert!(
        response["structuredContent"]["body"]["content"]
            .as_str()
            .is_some_and(|content| content.contains("pub fn alpha"))
    );
    assert!(
        response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("```text") && text.contains("pub fn alpha"))
    );

    let location_only = handle_tool_call(
        &provider,
        &json!({
            "name": "read_symbol",
            "arguments": { "symbol": "alpha", "include_body": false }
        }),
    )
    .expect("location-only read_symbol");
    assert!(location_only["structuredContent"].get("body").is_none());
    assert_eq!(
        location_only["structuredContent"]["matches"][0]["path"],
        Value::String("a.rs".to_string())
    );
    assert!(
        location_only["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("body omitted") && !text.contains("ambiguous"))
    );
}

#[test]
fn manage_selection_is_listed_dispatches_and_persists() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("text.txt"), "one\ntwo\n").expect("write");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"manage_selection"));

    let set_response = handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": {
                "op": "set",
                "paths": ["text.txt"],
                "mode": "full"
            }
        }),
    )
    .expect("selection dispatch");
    assert_eq!(
        set_response["structuredContent"]["files"][0]["path"],
        Value::String("text.txt".to_string())
    );
    assert!(
        set_response["structuredContent"]["files"][0]["token_estimate"]
            .as_u64()
            .expect("token count")
            > 0
    );

    let get_response = handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": { "op": "get" }
        }),
    )
    .expect("selection get");
    assert_eq!(
        get_response["structuredContent"]["files"][0]["path"],
        Value::String("text.txt".to_string())
    );
}

#[test]
fn manage_selection_previews_and_promotes_without_surprises() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("write");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let manage_selection = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .find(|tool| tool["name"] == "manage_selection")
        .expect("manage_selection spec");
    let ops = manage_selection["inputSchema"]["properties"]["op"]["enum"]
        .as_array()
        .expect("op enum");
    assert!(ops.contains(&Value::String("preview".to_string())));
    assert!(ops.contains(&Value::String("promote".to_string())));
    assert!(ops.contains(&Value::String("demote".to_string())));
    assert_eq!(
        manage_selection["inputSchema"]["properties"]["auto_codemap"]["default"],
        Value::Bool(false)
    );

    let preview = handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": { "op": "preview", "paths": ["lib.rs"], "mode": "codemap_only" }
        }),
    )
    .expect("preview");
    assert_eq!(preview["structuredContent"]["preview"], Value::Bool(true));
    assert_eq!(
        preview["structuredContent"]["would_mutate"],
        Value::Bool(true)
    );
    assert_eq!(
        preview["structuredContent"]["files"][0]["mode"],
        Value::String("codemap_only".to_string())
    );

    let persisted_after_preview = handle_tool_call(
        &provider,
        &json!({ "name": "manage_selection", "arguments": { "op": "get" } }),
    )
    .expect("get after preview");
    assert!(
        persisted_after_preview["structuredContent"]["files"]
            .as_array()
            .expect("files")
            .is_empty()
    );

    handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": { "op": "set", "paths": ["lib.rs"], "mode": "codemap_only" }
        }),
    )
    .expect("set codemap");
    let promoted = handle_tool_call(
        &provider,
        &json!({ "name": "manage_selection", "arguments": { "op": "promote", "paths": ["lib.rs"] } }),
    )
    .expect("promote");
    assert_eq!(promoted["structuredContent"]["mutated"], Value::Bool(true));
    assert_eq!(
        promoted["structuredContent"]["files"][0]["mode"],
        Value::String("full".to_string())
    );
}

#[test]
fn manage_selection_auto_codemap_dispatch_adds_reference_codemap() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.py"), "class Widget:\n    pass\n").expect("target");
    fs::write(
        dir.path().join("README.md"),
        "# Example\n\n```python\ndef example():\n    return Widget()\n```\n",
    )
    .expect("readme");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": {
                "op": "set",
                "paths": ["README.md"],
                "mode": "full",
                "auto_codemap": true
            }
        }),
    )
    .expect("auto codemap dispatch");
    let files = response["structuredContent"]["files"]
        .as_array()
        .expect("files");

    assert_eq!(response["structuredContent"]["auto_codemap_added"], 1);
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "README.md" && file["mode"] == "full")
    );
    assert!(
        files
            .iter()
            .any(|file| file["path"] == "target.py" && file["mode"] == "codemap_only")
    );
}

#[test]
fn workspace_context_is_listed_and_dispatches() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("text.txt"), "one\ntwo\n").expect("write");
    fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("write lib");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"workspace_context"));
    let workspace_context_spec = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .find(|tool| tool["name"] == "workspace_context")
        .expect("workspace_context spec");
    let include_values =
        workspace_context_spec["inputSchema"]["properties"]["include"]["items"]["enum"]
            .as_array()
            .expect("include enum");
    assert!(include_values.contains(&Value::String("tree".to_string())));
    assert!(include_values.contains(&Value::String("code".to_string())));

    handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": {
                "op": "set",
                "mode": "slices",
                "slices": [{
                    "path": "text.txt",
                    "ranges": [{ "start_line": 2, "end_line": 2, "description": "important line" }]
                }]
            }
        }),
    )
    .expect("selection dispatch");

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "workspace_context",
            "arguments": {
                "include": ["file-map", "contents", "tokens"],
                "instructions": "Use this context."
            }
        }),
    )
    .expect("workspace context dispatch");
    // The assembled context lives in content[].text; structuredContent keeps
    // only the token breakdown (the body is not duplicated across channels).
    let text = response["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("<file_map>"));
    assert!(text.contains("<tokens>"));
    assert!(text.contains("description=\"important line\""));
    let structured = &response["structuredContent"];
    assert!(structured["context"].is_null());
    assert_eq!(
        structured["tokens"]["files"][0]["path"],
        Value::String("text.txt".to_string())
    );
    assert_eq!(
        structured["tokens"]["files"][0]["segments"][0]["label"],
        Value::String("important line".to_string())
    );

    handle_tool_call(
        &provider,
        &json!({
            "name": "manage_selection",
            "arguments": { "op": "add", "paths": ["lib.rs"], "mode": "full" }
        }),
    )
    .expect("select lib");
    let tree_code = handle_tool_call(
        &provider,
        &json!({
            "name": "workspace_context",
            "arguments": { "include": ["tree", "code"] }
        }),
    )
    .expect("workspace context tree code");
    let text = tree_code["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("<file_tree>"));
    assert!(text.contains("<code_structure>"));
    assert!(text.contains("pub fn alpha()"));
    assert!(
        tree_code["structuredContent"]["tokens"]["tree_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
    );
    assert!(
        tree_code["structuredContent"]["tokens"]["code_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0)
    );
}

#[test]
fn read_file_summary_elides_markdown_fenced_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("README.md"),
        "# Example\n\n```rust\npub fn demo() {\n    let one = 1;\n    let two = 2;\n    let three = 3;\n    println!(\"{}\", one + two + three);\n}\n```\n",
    )
    .expect("write readme");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "read_file",
            "arguments": { "path": "README.md", "view": "summary" }
        }),
    )
    .expect("summary read");
    let structured = &response["structuredContent"];
    let text = response["content"][0]["text"].as_str().expect("text");

    assert_eq!(
        structured["language"],
        Value::String("markdown".to_string())
    );
    assert_eq!(structured["parsed"], Value::Bool(true));
    assert_eq!(structured["elided"], Value::Bool(true));
    assert!(text.contains("README.md:5-8"), "{text}");
}

#[test]
fn build_context_is_listed_dispatches_and_preserves_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("text.txt"), "needle\n").expect("write");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );
    let before = provider.selection();

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"build_context"));

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "build_context",
            "arguments": {
                "query": "needle",
                "token_budget": 100,
                "max_files": 1
            }
        }),
    )
    .expect("build context dispatch");
    let structured = &response["structuredContent"];
    assert_eq!(
        structured["manifest"]["included"][0]["path"],
        Value::String("text.txt".to_string())
    );
    assert_eq!(provider.selection(), before);
}

#[test]
fn build_context_manifest_explains_included_and_excluded_scores() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "needle\n").expect("write a");
    fs::write(dir.path().join("b.txt"), "needle\n").expect("write b");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "build_context",
            "arguments": {
                "query": "needle",
                "token_budget": 500,
                "max_files": 1
            }
        }),
    )
    .expect("build context dispatch");
    let manifest = &response["structuredContent"]["manifest"];
    let included = &manifest["included"][0];
    let excluded = &manifest["excluded"][0];
    let allocation_trace = manifest["allocation_trace"]
        .as_array()
        .expect("allocation trace");

    assert_eq!(allocation_trace.len(), 2);
    assert_eq!(allocation_trace[0]["path"], included["path"]);
    assert_eq!(
        allocation_trace[0]["result"],
        Value::String("included".to_string())
    );
    assert_eq!(
        allocation_trace[0]["reason"],
        Value::String("accepted".to_string())
    );
    assert_eq!(
        allocation_trace[0]["attempts"][0]["mode"],
        Value::String("full".to_string())
    );
    assert_eq!(
        allocation_trace[0]["attempts"][0]["accepted"],
        Value::Bool(true)
    );
    assert_eq!(allocation_trace[1]["path"], excluded["path"]);
    assert_eq!(
        allocation_trace[1]["result"],
        Value::String("excluded".to_string())
    );
    assert_eq!(
        allocation_trace[1]["reason"],
        Value::String("max_files".to_string())
    );
    assert!(
        allocation_trace[1]["attempts"]
            .as_array()
            .expect("attempts")
            .is_empty()
    );

    assert_eq!(included["score"], included["score_breakdown"]["total"]);
    assert_eq!(excluded["score"], excluded["score_breakdown"]["total"]);
    assert_eq!(
        included["score_breakdown"]["source"],
        Value::String("ranked".to_string())
    );
    assert!(
        included["score_breakdown"]["search"]
            .as_str()
            .expect("search score")
            .parse::<f64>()
            .expect("numeric search contribution")
            > 0.0
    );
}

#[test]
fn build_context_reports_sensitive_findings_without_secret_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("secrets.env"),
        "OPENAI_API_KEY=sk-proj-1234567890abcdefghijklmnopqrstuvwxyz\n",
    )
    .expect("write");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "build_context",
            "arguments": {
                "query": "OPENAI_API_KEY",
                "token_budget": 400,
                "max_files": 1
            }
        }),
    )
    .expect("build context dispatch");
    let findings = response["structuredContent"]["manifest"]["sensitive_findings"]
        .as_array()
        .expect("sensitive findings");
    let text = response["content"][0]["text"].as_str().expect("text");

    assert!(text.starts_with("warning:"), "{text}");
    assert!(
        findings
            .iter()
            .any(|finding| finding["kind"] == "openai_api_key")
    );
    assert!(findings.iter().all(|finding| {
        !finding["message"]
            .as_str()
            .expect("message")
            .contains("sk-proj")
    }));
}

#[test]
fn build_context_expands_embedded_markdown_references_by_language() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.py"), "class Widget:\n    pass\n").expect("target");
    fs::write(
        dir.path().join("README.md"),
        "# Example\n\n```python\ndef example():\n    return Widget()\n```\n",
    )
    .expect("readme");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );

    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "build_context",
            "arguments": {
                "query": "example",
                "seed_paths": ["README.md"],
                "token_budget": 1200,
                "max_files": 1
            }
        }),
    )
    .expect("build context dispatch");
    let manifest = &response["structuredContent"]["manifest"];
    let included = manifest["included"].as_array().expect("included");
    assert!(included.iter().any(|file| file["path"] == "README.md"));
    assert!(
        included
            .iter()
            .any(|file| { file["path"] == "target.py" && file["mode"] == "codemap_only" })
    );
    let allocation_trace = manifest["allocation_trace"]
        .as_array()
        .expect("allocation trace");
    let target_trace = allocation_trace
        .iter()
        .find(|trace| trace["path"] == "target.py")
        .expect("target.py trace");
    assert_eq!(
        target_trace["reason"],
        Value::String("reference_expansion".to_string())
    );
    assert_eq!(
        target_trace["attempts"][0]["mode"],
        Value::String("codemap_only".to_string())
    );
    assert_eq!(target_trace["attempts"][0]["accepted"], Value::Bool(true));
}
// ---- workspace.rs ----

#[test]
fn resolver_routes_explicit_workspace() {
    let left = tempfile::tempdir().expect("left tempdir");
    let right = tempfile::tempdir().expect("right tempdir");
    fs::write(left.path().join("left.txt"), "alpha\n").expect("left write");
    fs::write(right.path().join("right.txt"), "beta\n").expect("right write");

    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("left", Arc::new(provider_for(left.path())));
    registry.insert("right", Arc::new(provider_for(right.path())));

    let response = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": {
                "workspace": "right",
                "pattern": "beta",
                "mode": "content"
            }
        }),
    )
    .expect("workspace-routed search");

    assert_eq!(
        response["structuredContent"]["content_matches"][0]["path"],
        Value::String("right.txt".to_string())
    );
}

#[test]
fn singleton_provider_ignores_default_and_explicit_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("text.txt"), "needle\n").expect("write");
    let provider = provider_for(dir.path());

    let default_response = handle_tool_call(
        &provider,
        &json!({
            "name": "file_search",
            "arguments": { "pattern": "needle", "mode": "content" }
        }),
    )
    .expect("default singleton search");
    let explicit_response = handle_tool_call(
        &provider,
        &json!({
            "name": "file_search",
            "arguments": { "workspace": "anything", "pattern": "needle", "mode": "content" }
        }),
    )
    .expect("explicit singleton search");

    assert_eq!(
        default_response["structuredContent"]["content_matches"][0]["path"],
        explicit_response["structuredContent"]["content_matches"][0]["path"]
    );
}

#[test]
fn registry_without_workspace_is_ambiguous_when_multiple_exist() {
    let left = tempfile::tempdir().expect("left tempdir");
    let right = tempfile::tempdir().expect("right tempdir");
    fs::write(left.path().join("left.txt"), "alpha\n").expect("left write");
    fs::write(right.path().join("right.txt"), "beta\n").expect("right write");

    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("left", Arc::new(provider_for(left.path())));
    registry.insert("right", Arc::new(provider_for(right.path())));

    let err = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": { "pattern": "alpha", "mode": "content" }
        }),
    )
    .expect_err("ambiguous workspace should fail");

    assert!(matches!(
        err,
        DispatchError::Core(NerveError::AmbiguousWorkspace)
    ));
    assert_eq!(err.to_string(), "ambiguous: specify workspace");
}

#[test]
fn registry_singleton_default_routes_to_only_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("only.txt"), "needle\n").expect("write");

    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("only", Arc::new(provider_for(dir.path())));

    let response = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": { "pattern": "needle", "mode": "content" }
        }),
    )
    .expect("singleton registry default search");

    assert_eq!(
        response["structuredContent"]["content_matches"][0]["path"],
        Value::String("only.txt".to_string())
    );
}

#[test]
fn registry_blank_workspace_routes_to_only_workspace() {
    // A model may fill the optional `workspace` field with an empty string; that
    // must behave like omitting it (resolve to the sole workspace), not error.
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("only.txt"), "needle\n").expect("write");

    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("only", Arc::new(provider_for(dir.path())));

    let response = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": { "pattern": "needle", "mode": "content", "workspace": "" }
        }),
    )
    .expect("blank workspace routes to the only workspace");

    assert_eq!(
        response["structuredContent"]["content_matches"][0]["path"],
        Value::String("only.txt".to_string())
    );
}

#[test]
fn manage_workspaces_add_remove_and_routes_new_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("added.txt"),
        "dynamic
",
    )
    .expect("write");
    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();

    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"manage_workspaces"));

    let add = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "manage_workspaces",
            "arguments": {
                "op": "add",
                "name": "dynamic",
                "roots": [dir.path()]
            }
        }),
    )
    .expect("add workspace");
    assert_eq!(
        add["structuredContent"]["workspaces"][0]["name"],
        Value::String("dynamic".to_string())
    );

    let search = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": {
                "workspace": "dynamic",
                "pattern": "dynamic",
                "mode": "content"
            }
        }),
    )
    .expect("search added workspace");
    assert_eq!(
        search["structuredContent"]["content_matches"][0]["path"],
        Value::String("added.txt".to_string())
    );

    handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "manage_workspaces",
            "arguments": { "op": "remove", "name": "dynamic" }
        }),
    )
    .expect("remove workspace");
    let err = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": { "workspace": "dynamic", "pattern": "dynamic" }
        }),
    )
    .expect_err("removed workspace should not route");
    assert!(matches!(
        err,
        DispatchError::Core(NerveError::UnknownWorkspace(_))
    ));
}

#[test]
fn workspaces_keep_selection_and_search_isolated() {
    let left = tempfile::tempdir().expect("left tempdir");
    let right = tempfile::tempdir().expect("right tempdir");
    fs::write(left.path().join("left.txt"), "alpha\n").expect("left write");
    fs::write(right.path().join("right.txt"), "beta\n").expect("right write");

    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
    registry.insert("left", Arc::new(provider_for(left.path())));
    registry.insert("right", Arc::new(provider_for(right.path())));

    handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "manage_selection",
            "arguments": {
                "workspace": "left",
                "op": "set",
                "paths": ["left.txt"],
                "mode": "full"
            }
        }),
    )
    .expect("set left selection");
    handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "manage_selection",
            "arguments": {
                "workspace": "right",
                "op": "set",
                "paths": ["right.txt"],
                "mode": "full"
            }
        }),
    )
    .expect("set right selection");

    let left_selection = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "manage_selection",
            "arguments": { "workspace": "left", "op": "get" }
        }),
    )
    .expect("get left selection");
    let right_selection = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "manage_selection",
            "arguments": { "workspace": "right", "op": "get" }
        }),
    )
    .expect("get right selection");

    assert_eq!(
        left_selection["structuredContent"]["files"][0]["path"],
        Value::String("left.txt".to_string())
    );
    assert_eq!(
        right_selection["structuredContent"]["files"][0]["path"],
        Value::String("right.txt".to_string())
    );

    let wrong_workspace_search = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": { "workspace": "left", "pattern": "beta", "mode": "content" }
        }),
    )
    .expect("search left for right content");
    let right_search = handle_tool_call_with_resolver(
        &registry,
        &json!({
            "name": "file_search",
            "arguments": { "workspace": "right", "pattern": "beta", "mode": "content" }
        }),
    )
    .expect("search right content");

    assert_eq!(
        wrong_workspace_search["structuredContent"]["content_matches"]
            .as_array()
            .expect("left matches")
            .len(),
        0
    );
    assert_eq!(
        right_search["structuredContent"]["content_matches"][0]["path"],
        Value::String("right.txt".to_string())
    );
}
