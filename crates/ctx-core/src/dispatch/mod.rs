//! Transport-neutral MCP tool dispatch for the context engine.

mod args;
mod ast;
mod editing;
mod error;
mod git;
mod handlers;
mod specs;
mod text;

use args::*;
use ast::*;
use editing::*;
pub use error::{DispatchError, dispatch_error_json, dispatch_error_kind};
use git::run_git;
use handlers::dispatch_provider_tool;
pub use specs::tool_specs;
#[cfg(test)]
use text::{REPO_MAP_TEXT_BUDGET_CHARS, render_repo_map_text};
use text::{ToolText, tool_response, tool_response_text};

use crate::edit;
use crate::{
    CancelToken, CatalogProvider, CtxError, SingletonWorkspaceResolver, WorkspaceResolver,
    build_context_cancellable, get_code_structure, get_file_tree, get_repo_map_cancellable,
    manage_selection, read_file, search_snapshot_cancellable, workspace_context,
};
use serde_json::{Value, json};

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
pub trait DispatchProvider: CatalogProvider + Clone + Send + Sync + 'static {}
#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl<T> DispatchProvider for T where T: CatalogProvider + Clone + Send + Sync + 'static {}

#[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
pub trait DispatchProvider: CatalogProvider + Sync {}
#[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
impl<T> DispatchProvider for T where T: CatalogProvider + Sync {}

/// Dispatch one MCP `tools/call` params object and return the MCP tool response.
pub fn handle_tool_call<P>(provider: &P, params: &Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    handle_tool_call_cancellable(provider, params, &CancelToken::never())
}

/// Dispatch one MCP `tools/call` params object with cooperative cancellation.
pub fn handle_tool_call_cancellable<P>(
    provider: &P,
    params: &Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let resolver = SingletonWorkspaceResolver::new(provider);
    handle_tool_call_with_resolver_cancellable(&resolver, params, cancel)
}

/// Dispatch one MCP `tools/call` params object through a workspace resolver.
pub fn handle_tool_call_with_resolver<R>(
    resolver: &R,
    params: &Value,
) -> Result<Value, DispatchError>
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    handle_tool_call_with_resolver_cancellable(resolver, params, &CancelToken::never())
}

/// Dispatch one MCP `tools/call` params object through a workspace resolver with cancellation.
pub fn handle_tool_call_with_resolver_cancellable<R>(
    resolver: &R,
    params: &Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    cancel.check_cancelled()?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or(DispatchError::MissingToolName)?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    #[cfg(not(target_arch = "wasm32"))]
    if name == "manage_workspaces" {
        let args: crate::ManageWorkspacesRequest = serde_json::from_value(arguments)?;
        return tool_response_text(&resolver.manage_workspaces(args)?);
    }
    let workspace = workspace_arg(&arguments)?;
    let provider = resolver.resolve_workspace(workspace)?;
    dispatch_provider_tool(name, &*provider, arguments, cancel)
}
/// Decode one JSON tool-call params object and encode the tool response as JSON.
pub fn handle_tool_call_json<P>(provider: &P, request_json: &str) -> Result<String, DispatchError>
where
    P: DispatchProvider,
{
    handle_tool_call_json_cancellable(provider, request_json, &CancelToken::never())
}

/// Decode one JSON tool-call params object and encode the tool response as JSON,
/// returning a JSON error object for cooperative cancellation.
pub fn handle_tool_call_json_cancellable<P>(
    provider: &P,
    request_json: &str,
    cancel: &CancelToken,
) -> Result<String, DispatchError>
where
    P: DispatchProvider,
{
    let resolver = SingletonWorkspaceResolver::new(provider);
    handle_tool_call_json_with_resolver_cancellable(&resolver, request_json, cancel)
}

/// Decode one JSON tool-call params object and encode the tool response through a resolver.
pub fn handle_tool_call_json_with_resolver<R>(
    resolver: &R,
    request_json: &str,
) -> Result<String, DispatchError>
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    handle_tool_call_json_with_resolver_cancellable(resolver, request_json, &CancelToken::never())
}

/// Decode one JSON tool-call params object and encode the resolver-routed tool response,
/// returning a JSON error object for cooperative cancellation.
pub fn handle_tool_call_json_with_resolver_cancellable<R>(
    resolver: &R,
    request_json: &str,
    cancel: &CancelToken,
) -> Result<String, DispatchError>
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    let params: Value = serde_json::from_str(request_json)?;
    match handle_tool_call_with_resolver_cancellable(resolver, &params, cancel) {
        Ok(response) => Ok(serde_json::to_string(&response)?),
        Err(err) if matches!(err, DispatchError::Core(CtxError::Cancelled)) => Ok(
            dispatch_error_json(dispatch_error_kind(&err), &err.to_string()),
        ),
        Err(err) => Err(err),
    }
}
fn workspace_arg(arguments: &Value) -> Result<Option<&str>, DispatchError> {
    arguments
        .get("workspace")
        .map(|value| {
            value.as_str().ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "workspace must be a string",
                ))
            })
        })
        .transpose()
        .map_err(DispatchError::Json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions, WorkspaceRegistry};
    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    use crate::{HostFile, MemoryCatalogProvider, semantic::SemanticIndex};
    use std::{fs, sync::Arc};

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    #[test]
    fn semantic_search_is_listed_when_feature_enabled() {
        let specs = tool_specs();
        let names: Vec<_> = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"semantic_search"));
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    #[test]
    fn semantic_search_without_runtime_index_is_unavailable() {
        let provider =
            MemoryCatalogProvider::new(vec![HostFile::new("a.rs", b"fn alpha() {}".to_vec())])
                .expect("provider");
        let err = handle_tool_call(
            &provider,
            &json!({
                "name": "semantic_search",
                "arguments": { "query": "alpha" }
            }),
        )
        .expect_err("semantic unavailable");
        assert!(matches!(
            err,
            DispatchError::Core(CtxError::SemanticUnavailable)
        ));
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    #[test]
    fn semantic_search_dispatches_with_mock_index() {
        let provider = MemoryCatalogProvider::new(vec![
            HostFile::new("config.rs", b"pub fn validate_config() {}".to_vec()),
            HostFile::new("view.rs", b"pub fn render_view() {}".to_vec()),
        ])
        .expect("provider");
        provider.set_semantic_index(Some(Arc::new(SemanticIndex::mock())));
        let response = handle_tool_call(
            &provider,
            &json!({
                "name": "semantic_search",
                "arguments": { "query": "config validation", "max_results": "1" }
            }),
        )
        .expect("semantic search");
        assert_eq!(
            response["structuredContent"]["results"][0]["path"],
            Value::String("config.rs".to_string())
        );
        assert!(
            response["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("semantic matches:")
        );
    }

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
        let parsed =
            serde_json::from_value::<ReadFileArgs>(json!({ "path": "a.txt", "limit": "abc" }));
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
        assert_eq!(response["structuredContent"]["total_lines"], json!(3));
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

    fn provider_for(path: &std::path::Path) -> FsCatalogProvider {
        FsCatalogProvider::new(
            RootPolicy::new(vec![path.to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        )
    }

    // Regression: every numeric tool parameter must tolerate integer-valued
    // strings (clients that stringify numbers), per the documented contract.
    // build_context.token_budget/max_files and ast_search.max_results were the
    // two holdouts.
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
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return; // git not installed; skip
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
        };
        git(&["init", "-q"]);
        fs::write(dir.path().join("a.txt"), "one\ntwo\n").expect("seed");
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "init"]);
        let provider = provider_for(dir.path());

        // edit response carries a unified diff of exactly this change
        let res = handle_tool_call(
            &provider,
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

        // git diff sees the working-tree change
        let g = handle_tool_call(
            &provider,
            &json!({ "name": "git", "arguments": { "op": "diff" } }),
        )
        .expect("git diff");
        assert!(
            g["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .contains("+TWO"),
            "git diff output"
        );

        // git status lists the modified file
        let s = handle_tool_call(
            &provider,
            &json!({ "name": "git", "arguments": { "op": "status" } }),
        )
        .expect("git status");
        assert!(
            s["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .contains("a.txt")
        );
    }

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
            DispatchError::Core(CtxError::AmbiguousWorkspace)
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
            DispatchError::Core(CtxError::UnknownWorkspace(_))
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
    fn workspace_context_is_listed_and_dispatches() {
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
        assert!(tool_names.contains(&"workspace_context"));

        handle_tool_call(
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
        let structured = &response["structuredContent"];
        assert!(structured["context"].is_null());
        assert_eq!(
            structured["tokens"]["files"][0]["path"],
            Value::String("text.txt".to_string())
        );
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
}
