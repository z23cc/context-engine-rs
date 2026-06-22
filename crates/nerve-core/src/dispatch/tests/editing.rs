use super::*;

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
fn apply_patch_duplicate_create_fails_preflight_without_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "new.txt".to_string(),
            content: "one\n".to_string(),
        },
        edit::FileChange::Create {
            path: "new.txt".to_string(),
            content: "two\n".to_string(),
        },
    ];
    let err = apply_changes(&provider, changes, DiffOptions::default(), false)
        .err()
        .expect("duplicate create preflight");
    assert!(err.to_string().contains("duplicate create/update target"));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert!(!dir.path().join("new.txt").exists());
}

#[test]
fn create_over_existing_fails_preflight_before_later_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::write(dir.path().join("exists.txt"), "old\n").expect("seed exists");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Create {
            path: "exists.txt".to_string(),
            content: "new\n".to_string(),
        },
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
    ];
    let err = apply_changes(&provider, changes, DiffOptions::default(), false)
        .err()
        .expect("existing destination preflight");
    assert!(err.to_string().contains("destination already exists"));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("exists.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn atomic_true_unsupported_provider_fails_before_mutation() {
    #[derive(Default)]
    struct BasicProvider(std::sync::RwLock<std::collections::BTreeMap<String, String>>);
    impl CatalogProvider for BasicProvider {
        fn snapshot(&self) -> Result<crate::CatalogSnapshot, NerveError> {
            Ok(crate::CatalogSnapshot {
                generation: 0,
                roots: vec![],
                entries: vec![],
                diagnostics: vec![],
            })
        }
        fn read_bytes(&self, path: &std::path::Path) -> Result<Vec<u8>, NerveError> {
            self.0
                .read()
                .unwrap()
                .get(&path.to_string_lossy().to_string())
                .map(|text| text.as_bytes().to_vec())
                .ok_or_else(|| NerveError::OutsideRoots(path.to_path_buf()))
        }
        fn write_text(&self, path: &std::path::Path, content: &str) -> Result<(), NerveError> {
            self.0
                .write()
                .unwrap()
                .insert(path.to_string_lossy().to_string(), content.to_string());
            Ok(())
        }
    }
    let provider = BasicProvider::default();
    provider
        .write_text(std::path::Path::new("a.txt"), "alpha\n")
        .unwrap();
    let changes = vec![edit::FileChange::Update {
        path: "a.txt".to_string(),
        content: "ALPHA\n".to_string(),
    }];
    let err = apply_changes(&provider, changes, DiffOptions::default(), true)
        .err()
        .expect("atomic unsupported");
    assert!(matches!(
        err,
        DispatchError::Core(NerveError::AtomicBatchUnsupported)
    ));
    assert_eq!(
        provider.read_bytes(std::path::Path::new("a.txt")).unwrap(),
        b"alpha\n"
    );
}

#[test]
fn non_atomic_preflight_rejects_invalid_create_before_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::write(dir.path().join("not_dir"), "file\n").expect("seed blocker");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "not_dir/new.txt".to_string(),
            content: "new\n".to_string(),
        },
    ];
    apply_changes(&provider, changes, DiffOptions::default(), false)
        .err()
        .expect("invalid destination preflight");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
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

#[test]
fn fs_atomic_backup_collision_preserves_user_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::create_dir(dir.path().join("existing_dir")).expect("seed blocker");
    let backup_name = format!(".a.txt.ctx-bak-{}-0-0", std::process::id());
    fs::write(dir.path().join(&backup_name), "do not delete\n").expect("seed backup collision");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "existing_dir".to_string(),
            content: "new\n".to_string(),
        },
    ];
    apply_changes(&provider, changes, DiffOptions::default(), true)
        .err()
        .expect("atomic rollback");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(backup_name)).unwrap(),
        "do not delete\n"
    );
}

#[test]
fn fs_atomic_rollback_restores_first_write_after_later_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::create_dir(dir.path().join("existing_dir")).expect("seed blocker");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "existing_dir".to_string(),
            content: "new\n".to_string(),
        },
    ];
    let err = apply_changes(&provider, changes, DiffOptions::default(), true)
        .err()
        .expect("atomic rollback");
    assert!(matches!(
        err,
        DispatchError::Core(NerveError::AtomicBatchFailed { .. })
    ));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert!(dir.path().join("existing_dir").is_dir());
}
