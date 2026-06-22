use super::*;

#[test]
fn stale_hash_error_has_structured_reread_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "current\n").expect("seed");
    let provider = provider_for(dir.path());
    let patch = "*** Begin Patch\n[a.txt#0000]\nSWAP 1.=1:\n+x\n*** End Patch\n";
    let err = handle_tool_call(
        &provider,
        &json!({ "name": "edit", "arguments": { "mode": "hashline", "patch": patch } }),
    )
    .expect_err("stale hash");
    let value = dispatch_error_value(&err);
    assert_eq!(value["error"]["kind"], json!("stale_hash"));
    assert_eq!(value["error"]["path"], json!("a.txt"));
    assert_eq!(value["error"]["expected_hash"], json!("0000"));
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
