use super::*;

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
    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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
fn symbolic_edit_tools_are_listed() {
    let specs = tool_specs();
    for name in [
        "replace_symbol_body",
        "insert_before_symbol",
        "insert_after_symbol",
    ] {
        let tool = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("{name} spec"));
        let required = tool["inputSchema"]["required"]
            .as_array()
            .expect("required");
        assert!(required.contains(&Value::String("symbol".to_string())));
        assert!(required.contains(&Value::String("body".to_string())));
    }
}

#[test]
fn rename_symbol_is_listed() {
    let specs = tool_specs();
    let tool = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("rename_symbol"))
        .expect("rename_symbol spec");
    let required = tool["inputSchema"]["required"]
        .as_array()
        .expect("required");
    assert!(required.contains(&Value::String("symbol".to_string())));
    assert!(required.contains(&Value::String("new_name".to_string())));
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
        included["score_breakdown"]["semantic_enabled"],
        Value::Bool(false)
    );
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
