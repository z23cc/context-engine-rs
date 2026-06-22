use super::*;

#[test]
fn read_file_args_accept_string_numbers() {
    let args: ReadFileArgs =
        serde_json::from_value(json!({ "path": "a.txt", "limit": "130" })).expect("parse");
    assert_eq!(args.limit, Some(130));
    assert_eq!(args.start_line, None);
}

#[test]
fn read_file_args_accept_numeric_and_offset_alias() {
    let args: ReadFileArgs =
        serde_json::from_value(json!({ "path": "a.txt", "offset": 5, "limit": 10 }))
            .expect("parse");
    assert_eq!(args.start_line, Some(5));
    assert_eq!(args.limit, Some(10));
}

#[test]
fn read_file_args_treat_null_and_absent_as_none() {
    let args: ReadFileArgs =
        serde_json::from_value(json!({ "path": "a.txt", "start_line": null })).expect("parse");
    assert_eq!(args.start_line, None);
    assert_eq!(args.end_line, None);
    assert_eq!(args.limit, None);
}

#[test]
fn read_file_args_reject_non_numeric_string() {
    let parsed = serde_json::from_value::<ReadFileArgs>(json!({ "path": "a.txt", "limit": "abc" }));
    assert!(parsed.is_err());
}

#[test]
fn file_search_args_accept_string_numbers_and_keep_defaults() {
    let args: FileSearchArgs = serde_json::from_value(
        json!({ "pattern": "x", "max_results": "10", "max_content_bytes": "2048" }),
    )
    .expect("parse");
    assert_eq!(args.max_results, 10);
    assert_eq!(args.max_content_bytes, 2048);
    assert_eq!(args.context_lines, 2);
}

#[test]
fn tool_text_read_file_is_raw_content() {
    let response = crate::ReadFileResponse {
        path: "a.txt".into(),
        display_path: "a.txt".to_string(),
        first_line: 1,
        last_line: 2,
        total_lines: 2,
        content: "one\ntwo\n".to_string(),
        snap: None,
    };
    assert_eq!(response.tool_text(), "one\ntwo\n");
}

#[test]
fn tool_text_file_tree_is_ascii() {
    let response = crate::FileTreeResponse {
        roots: vec![],
        tree: "src/\n  lib.rs\n".to_string(),
        roots_count: 1,
        was_truncated: false,
        uses_legend: false,
        omitted: 0,
        note: None,
    };
    assert_eq!(response.tool_text(), "src/\n  lib.rs\n");
}

#[test]
fn tool_text_code_structure_lists_symbols() {
    let response = crate::codemap::CodeStructureResponse {
        files: vec![crate::codemap::FileCodeStructure {
            path: "src/lib.rs".to_string(),
            language: "rust".to_string(),
            symbols: vec![crate::codemap::CodeSymbol {
                kind: "function".to_string(),
                name: "needle".to_string(),
                line: 12,
                column: 8,
                signature: None,
                members: vec![],
            }],
            token_count: 0,
        }],
        diagnostics: vec![],
        omitted: 0,
        total_tokens: 0,
    };
    let text = response.tool_text();
    assert!(text.contains("src/lib.rs"));
    assert!(text.contains("function needle (12)"));
    assert!(!text.contains("\"symbols\""));
}

#[test]
fn read_file_content_text_is_raw_not_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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
    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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
fn repo_map_text_degrades_to_budget() {
    use crate::repomap::{RepoMapFile, RepoMapResponse, RepoMapTotals};
    let files: Vec<RepoMapFile> = (0..10)
        .map(|i| RepoMapFile {
            rank: i + 1,
            path: format!("src/file_{i:02}.rs"),
            display_path: format!("src/file_{i:02}.rs"),
            language: "rust".to_string(),
            score: format!("0.{i:08}"),
            symbols: Vec::new(),
        })
        .collect();
    let response = RepoMapResponse {
        files,
        diagnostics: Vec::new(),
        totals: RepoMapTotals {
            scanned_files: 10,
            indexed_files: 10,
            symbols_indexed: 0,
            edges: 0,
            seed_files: 0,
            omitted_files: 0,
            max_files: 10,
            damping: "0.85".to_string(),
            iterations: 30,
        },
        reference_heuristic: String::new(),
    };
    // Tiny budget: only the top-ranked file fits, the rest are noted.
    let text = render_repo_map_text(&response, 40);
    assert!(text.contains("src/file_00.rs"));
    assert!(!text.contains("src/file_09.rs"));
    assert!(text.contains("more ranked files omitted"));
    // Full budget renders every file with no omission note.
    let full = render_repo_map_text(&response, REPO_MAP_TEXT_BUDGET_CHARS);
    assert!(full.contains("src/file_09.rs"));
    assert!(!full.contains("omitted"));
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
