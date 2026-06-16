use ctx_core::{
    BuildContextRequest, CatalogProvider, FsCatalogProvider, LineRange, ManageSelectionMode,
    ManageSelectionOp, ManageSelectionRequest, ReadFileRequest, RepoMapRequest, RootPolicy,
    ScanOptions, SearchMode, SearchRequest, SelectionSliceArg, WorkspaceContextInclude,
    WorkspaceContextRequest, build_context, get_code_structure, get_file_tree, get_repo_map,
    handle_tool_call, manage_selection, read_file, search_snapshot, tool_specs, workspace_context,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn provider() -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![fixture_root()]).expect("root policy"),
        ScanOptions::default(),
    )
}

fn snapshot() -> (FsCatalogProvider, ctx_core::CatalogSnapshot) {
    let provider = provider();
    let snapshot = provider.snapshot().expect("snapshot");
    (provider, snapshot)
}

fn normalize_root_names(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("path") == Some(&Value::String(String::new()))
                && map.get("kind") == Some(&Value::String("directory".to_string()))
            {
                map.insert(
                    "name".to_string(),
                    Value::String("<fixture-root>".to_string()),
                );
            }
            for child in map.values_mut() {
                normalize_root_names(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_root_names(item);
            }
        }
        _ => {}
    }
}

fn normalize_read_paths(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("path") && map.contains_key("display_path") {
                map.insert(
                    "path".to_string(),
                    Value::String("<absolute-path>".to_string()),
                );
            }
            for child in map.values_mut() {
                normalize_read_paths(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_read_paths(item);
            }
        }
        _ => {}
    }
}

fn normalize_tree_ascii(value: &mut Value) {
    if let Value::Object(map) = value
        && let Some(Value::String(tree)) = map.get_mut("tree")
    {
        let root_name = fixture_root()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        *tree = tree.replacen(&root_name, "<fixture-root>", 1);
    }
}

#[test]
fn golden_file_search_literal_regex_and_path() {
    let (provider, snapshot) = snapshot();
    let literal = search_snapshot(
        &provider,
        &snapshot,
        &SearchRequest {
            pattern: "needle".to_string(),
            mode: SearchMode::Both,
            max_results: 20,
            context_lines: 1,
            ..SearchRequest::default()
        },
    )
    .expect("literal search");
    let regex = search_snapshot(
        &provider,
        &snapshot,
        &SearchRequest {
            pattern: r"pub\s+(struct|enum)".to_string(),
            mode: SearchMode::Content,
            regex: true,
            max_results: 20,
            context_lines: 1,
            ..SearchRequest::default()
        },
    )
    .expect("regex search");
    let path = search_snapshot(
        &provider,
        &snapshot,
        &SearchRequest {
            pattern: "nested".to_string(),
            mode: SearchMode::Path,
            max_results: 20,
            context_lines: 0,
            ..SearchRequest::default()
        },
    )
    .expect("path search");

    insta::assert_json_snapshot!(json!({
        "literal": literal,
        "regex": regex,
        "path": path,
    }));
}

#[test]
fn golden_read_file_whole_and_slice() {
    let provider = provider();
    let whole = read_file(
        &provider,
        &ReadFileRequest {
            path: PathBuf::from("notes.txt"),
            start_line: None,
            end_line: None,
            limit: None,
        },
    )
    .expect("whole read");
    let slice = read_file(
        &provider,
        &ReadFileRequest {
            path: PathBuf::from("notes.txt"),
            start_line: Some(2),
            end_line: None,
            limit: Some(1),
        },
    )
    .expect("slice read");

    let mut value = json!({
        "whole": whole,
        "slice": slice,
    });
    normalize_read_paths(&mut value);
    insta::assert_json_snapshot!(value);
}

#[test]
fn golden_read_file_summary_view() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("summary-root");
    fs::create_dir(&root).expect("create root");
    fs::write(
        root.join("summary_sample.ts"),
        "import alpha from 'alpha';\nimport beta from 'beta';\nimport gamma from 'gamma';\nimport delta from 'delta';\nimport epsilon from 'epsilon';\n\nexport function greet(name: string): string {\n    const clean = name.trim();\n    const label = clean || 'world';\n    return `hello ${label}`;\n}\n",
    )
    .expect("write summary sample");
    fs::write(
        root.join("summary_broken.ts"),
        "export function broken( {\n    return 1;\n",
    )
    .expect("write broken sample");
    fs::write(root.join("summary_notes.txt"), "plain text\nwith lines\n")
        .expect("write unsupported sample");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![root]).expect("root policy"),
        ScanOptions::default(),
    );
    let summary = handle_tool_call(
        &provider,
        &json!({ "name": "read_file", "arguments": {
            "path": "summary_sample.ts", "view": "summary"
        } }),
    )
    .expect("summary read");
    let fallback = handle_tool_call(
        &provider,
        &json!({ "name": "read_file", "arguments": {
            "path": "summary_broken.ts", "view": "summary"
        } }),
    )
    .expect("fallback read");
    let unsupported = handle_tool_call(
        &provider,
        &json!({ "name": "read_file", "arguments": {
            "path": "summary_notes.txt", "view": "summary"
        } }),
    )
    .expect("unsupported read");

    insta::assert_json_snapshot!(json!({
        "summary": summary,
        "fallback": fallback,
        "unsupported": unsupported,
    }));
}

#[test]
fn golden_get_file_tree() {
    let (_provider, snapshot) = snapshot();
    let options = ctx_core::FileTreeOptions {
        mode: ctx_core::TreeMode::Full,
        max_depth: Some(4),
        path: None,
    };
    let mut value = serde_json::to_value(get_file_tree(&snapshot, &options)).expect("tree json");
    normalize_root_names(&mut value);
    normalize_tree_ascii(&mut value);
    insta::assert_json_snapshot!(value);
}

#[test]
fn golden_get_code_structure() {
    let (provider, snapshot) = snapshot();
    let response = get_code_structure(&provider, &snapshot, &[]).expect("code structure");
    insta::assert_json_snapshot!(response);
}

#[test]
fn golden_get_repo_map() {
    let (provider, snapshot) = snapshot();
    let response = get_repo_map(
        &provider,
        &snapshot,
        &RepoMapRequest {
            query: Some("shared_rust_helper".to_string()),
            seed_paths: vec![PathBuf::from("repo_map/rust_consumer.rs")],
            max_files: 5,
        },
    )
    .expect("repo map");
    insta::assert_json_snapshot!(response);
}

#[test]
fn golden_workspace_context() {
    let (provider, snapshot) = snapshot();
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("notes.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
        },
    )
    .expect("select full");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: Vec::new(),
            mode: Some(ManageSelectionMode::Slices),
            slices: vec![SelectionSliceArg {
                path: PathBuf::from("nested/beta.rs"),
                ranges: vec![LineRange {
                    start_line: 1,
                    end_line: 2,
                }],
            }],
        },
    )
    .expect("select slices");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: vec![PathBuf::from("alpha.rs")],
            mode: Some(ManageSelectionMode::CodemapOnly),
            slices: Vec::new(),
        },
    )
    .expect("select codemap");

    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            include: vec![
                WorkspaceContextInclude::FileMap,
                WorkspaceContextInclude::Contents,
            ],
            instructions: Some("Answer from selected files only.".to_string()),
        },
    )
    .expect("workspace context");
    insta::assert_json_snapshot!(response);
}

#[test]
fn golden_build_context() {
    let (provider, snapshot) = snapshot();
    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "Alpha".to_string(),
            token_budget: 120,
            max_files: Some(4),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");
    insta::assert_json_snapshot!(response);
}

#[test]
fn get_repo_map_is_listed_and_dispatches() {
    let specs = tool_specs();
    let tool_names: Vec<_> = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert!(tool_names.contains(&"get_repo_map"));

    let provider = provider();
    let response = handle_tool_call(
        &provider,
        &json!({
            "name": "get_repo_map",
            "arguments": {
                "query": "shared_rust_helper",
                "max_files": 1
            }
        }),
    )
    .expect("repo-map dispatch");
    assert_eq!(
        response["structuredContent"]["files"][0]["path"],
        Value::String("repo_map/rust_lib.rs".to_string())
    );
}
