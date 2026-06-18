use super::*;

#[test]
fn resolver_routes_explicit_workspace() {
    let left = tempfile::tempdir().expect("left tempdir");
    let right = tempfile::tempdir().expect("right tempdir");
    fs::write(left.path().join("left.txt"), "alpha\n").expect("left write");
    fs::write(right.path().join("right.txt"), "beta\n").expect("right write");

    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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

    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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

    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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

    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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
    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();

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

    let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
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
