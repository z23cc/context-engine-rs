use ctx_core::{
    CatalogProvider, FsCatalogProvider, ReadFileRequest, RepoMapRequest, RootPolicy, ScanOptions,
    SearchMode, SearchRequest, get_code_structure, get_file_tree, get_repo_map, handle_tool_call,
    read_file, search_snapshot, tool_specs,
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
            regex: false,
            max_results: 20,
            context_lines: 1,
            max_content_files: 2_048,
            max_content_bytes: 64 * 1024 * 1024,
            whole_word: false,
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
            max_content_files: 2_048,
            max_content_bytes: 64 * 1024 * 1024,
            whole_word: false,
        },
    )
    .expect("regex search");
    let path = search_snapshot(
        &provider,
        &snapshot,
        &SearchRequest {
            pattern: "nested".to_string(),
            mode: SearchMode::Path,
            regex: false,
            max_results: 20,
            context_lines: 0,
            max_content_files: 2_048,
            max_content_bytes: 64 * 1024 * 1024,
            whole_word: false,
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
fn golden_get_file_tree() {
    let (_provider, snapshot) = snapshot();
    let mut value = serde_json::to_value(get_file_tree(&snapshot, 4)).expect("tree json");
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
