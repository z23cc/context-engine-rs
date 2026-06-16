use ctx_core::{
    CatalogProvider, FsCatalogProvider, ReadFileRequest, RepoMapRequest, RootPolicy, ScanOptions,
    SearchMode, SearchRequest, get_code_structure, get_file_tree, get_repo_map,
    handle_tool_call_json, read_file, search_snapshot,
};
use proptest::prelude::*;
use serde_json::json;
use std::{fs, path::Path};

fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dirs");
    }
    fs::write(path, bytes).expect("write file");
}

fn provider_for(root: &Path) -> (FsCatalogProvider, ctx_core::CatalogSnapshot) {
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![root.to_path_buf()]).expect("root policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");
    (provider, snapshot)
}

fn search_request(pattern: String, mode: SearchMode) -> SearchRequest {
    SearchRequest {
        pattern,
        mode,
        max_results: 20,
        context_lines: 3,
        max_content_files: 128,
        max_content_bytes: 2 * 1024 * 1024,
        ..SearchRequest::default()
    }
}

fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn codemap_parsers_do_not_panic_on_arbitrary_source(bytes in prop::collection::vec(any::<u8>(), 0..4096)) {
        let dir = tempfile::tempdir().expect("tempdir");
        for rel in ["bad.rs", "bad.py", "bad.js", "bad.ts", "bad.tsx"] {
            write_file(dir.path(), rel, &bytes);
        }
        let (provider, snapshot) = provider_for(dir.path());
        let response = get_code_structure(&provider, &snapshot, &[]);
        prop_assert!(response.is_ok());
    }

    #[test]
    fn literal_search_modes_do_not_panic_on_random_bytes(
        haystack in prop::collection::vec(any::<u8>(), 0..4096),
        needle in prop::collection::vec(any::<u8>(), 0..64),
        mode_idx in 0usize..3,
        whole_word in any::<bool>(),
        context_lines in 0usize..8,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "blob.bin", &haystack);
        write_file(dir.path(), "text.txt", &haystack);
        let (provider, snapshot) = provider_for(dir.path());
        let mode = match mode_idx {
            0 => SearchMode::Path,
            1 => SearchMode::Content,
            _ => SearchMode::Both,
        };
        let mut request = search_request(lossy(needle), mode);
        request.whole_word = whole_word;
        request.context_lines = context_lines;
        let result = search_snapshot(&provider, &snapshot, &request);
        prop_assert!(result.is_ok());
    }

    #[test]
    fn regex_search_reports_invalid_patterns_without_panic(
        haystack in prop::collection::vec(any::<u8>(), 0..2048),
        pattern in prop::collection::vec(any::<u8>(), 0..96),
        mode_idx in 0usize..3,
        whole_word in any::<bool>(),
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "text.txt", &haystack);
        let (provider, snapshot) = provider_for(dir.path());
        let mode = match mode_idx {
            0 => SearchMode::Path,
            1 => SearchMode::Content,
            _ => SearchMode::Both,
        };
        let mut request = search_request(lossy(pattern), mode);
        request.regex = true;
        request.whole_word = whole_word;
        let result = search_snapshot(&provider, &snapshot, &request);
        prop_assert!(result.is_ok() || matches!(result, Err(ctx_core::CtxError::InvalidRegex(_))));
    }

    #[test]
    fn repo_map_comment_and_string_scanner_does_not_panic_on_random_payload(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
        let dir = tempfile::tempdir().expect("tempdir");
        let text = lossy(payload);
        let escaped = text.escape_default().to_string();
        write_file(
            dir.path(),
            "lib.rs",
            format!("pub struct Target;\npub fn caller() {{ let _ = Target; let _s = r#\"{}\"#; }}\n// {}\n", escaped, escaped).as_bytes(),
        );
        write_file(
            dir.path(),
            "script.py",
            format!("class PyTarget:\n    pass\ndef use_py():\n    return '''{}'''\n# {}\n", escaped, escaped).as_bytes(),
        );
        let js_string = serde_json::to_string(&text).expect("json string");
        write_file(
            dir.path(),
            "script.js",
            format!("export class JsTarget {{}}\nexport function useJs() {{ return {js_string}; }}\n// {escaped}\n").as_bytes(),
        );
        let (provider, snapshot) = provider_for(dir.path());
        let result = get_repo_map(&provider, &snapshot, &RepoMapRequest::default());
        prop_assert!(result.is_ok());
    }

    #[test]
    fn json_dispatch_does_not_panic_on_random_or_malformed_input(input in prop::collection::vec(any::<u8>(), 0..2048)) {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "text.txt", b"needle\n");
        let (provider, _snapshot) = provider_for(dir.path());
        let request = lossy(input);
        let result = handle_tool_call_json(&provider, &request);
        if serde_json::from_str::<serde_json::Value>(&request).is_err() {
            prop_assert!(result.is_err());
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)] // reason: table-driven boundary corpus fixture exercises many degradation cases.
fn boundary_corpus_uses_tempdir_and_degrades_gracefully() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_file(dir.path(), "empty.txt", b"");
    write_file(dir.path(), "no_newline.txt", b"last line has no newline");
    write_file(dir.path(), "crlf.txt", b"one\r\ntwo needle\r\nthree\r\n");
    write_file(dir.path(), "binary.bin", b"needle\0not text\xff\xfe");
    write_file(
        dir.path(),
        "long.txt",
        format!("needle {}\n", "x".repeat(400)).as_bytes(),
    );
    write_file(
        dir.path(),
        "big.txt",
        format!("{}needle\n", "filler\n".repeat(20_000)).as_bytes(),
    );
    write_file(
        dir.path(),
        "deep/a/b/c/d/e/file.rs",
        b"pub fn deep_symbol() {}\n",
    );
    write_file(dir.path(), "bad.rs", b"pub fn { not valid rust");
    write_file(dir.path(), "bad.py", b"def broken(:\n");
    write_file(dir.path(), "bad.js", b"export function ( {\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let _ = symlink(dir.path(), dir.path().join("deep/a/b/c/d/e/loop"));
    }

    let (provider, snapshot) = provider_for(dir.path());
    let tree = get_file_tree(
        &snapshot,
        &ctx_core::FileTreeOptions {
            mode: ctx_core::TreeMode::Full,
            max_depth: Some(20),
            path: None,
        },
    );
    assert!(!tree.roots.is_empty());

    let codemap = get_code_structure(&provider, &snapshot, &[]).expect("codemap response");
    // tree-sitter recovers from malformed source instead of producing parse
    // errors, so the codemap degrades gracefully: the bad.* files do not crash
    // extraction and the valid deep file is still indexed with its symbol.
    let deep = codemap
        .files
        .iter()
        .find(|file| file.path == "deep/a/b/c/d/e/file.rs")
        .expect("deep file indexed");
    assert!(
        deep.symbols
            .iter()
            .any(|symbol| symbol.name == "deep_symbol")
    );

    let mut search = search_request("needle".to_string(), SearchMode::Both);
    search.context_lines = 2;
    let search_response = search_snapshot(&provider, &snapshot, &search).expect("search response");
    assert!(search_response.totals.binary_files_skipped >= 1);

    let mut budgeted_search = search_request("needle".to_string(), SearchMode::Content);
    budgeted_search.max_content_bytes = 1024;
    let budgeted_response =
        search_snapshot(&provider, &snapshot, &budgeted_search).expect("budgeted search response");
    assert!(budgeted_response.totals.totals_are_lower_bound);
    assert!(
        search_response
            .content_matches
            .iter()
            .all(|hit| hit.text.chars().count() <= 240)
    );
    assert!(
        search_response
            .content_matches
            .iter()
            .flat_map(|hit| &hit.context)
            .all(|ctx| ctx.text.chars().count() <= 240)
    );

    let empty = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("empty.txt"),
            start_line: Some(usize::MAX),
            end_line: Some(usize::MAX),
            limit: Some(usize::MAX),
            snap: None,
        },
    )
    .expect("empty read");
    assert_eq!(empty.content, "");
    assert_eq!(empty.total_lines, 0);

    let no_newline = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("no_newline.txt"),
            start_line: Some(1),
            end_line: None,
            limit: None,
            snap: None,
        },
    )
    .expect("no newline read");
    assert_eq!(no_newline.content, "last line has no newline");

    let repo_map = get_repo_map(
        &provider,
        &snapshot,
        &RepoMapRequest {
            query: Some("deep_symbol".to_string()),
            seed_paths: vec!["deep/a/b/c/d/e".into()],
            max_files: 10,
        },
    )
    .expect("repo map response");
    assert!(repo_map.totals.scanned_files >= 1);

    let malformed_json = "{not json";
    let err = handle_tool_call_json(&provider, malformed_json).expect_err("malformed json errors");
    assert!(
        err.to_string().contains("expected object key")
            || err.to_string().contains("key must be a string")
    );

    let missing_name = json!({ "arguments": {} }).to_string();
    let err = handle_tool_call_json(&provider, &missing_name).expect_err("missing name errors");
    assert!(err.to_string().contains("requires string name"));

    let invalid_regex = json!({
        "name": "file_search",
        "arguments": { "pattern": "(", "regex": true, "mode": "both" }
    })
    .to_string();
    let err =
        handle_tool_call_json(&provider, &invalid_regex).expect_err("invalid regex is an error");
    assert!(err.to_string().contains("invalid regex"));
}
