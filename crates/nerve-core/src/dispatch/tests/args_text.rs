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
