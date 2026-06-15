//! Transport-neutral MCP tool dispatch for the context engine.

use crate::{
    CancelToken, CatalogProvider, CtxError, ReadFileRequest, RepoMapRequest, SearchMode,
    SearchRequest, get_code_structure, get_file_tree, get_repo_map_cancellable, read_file,
    search_snapshot_cancellable,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Errors produced while decoding or dispatching a tool call.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("tools/call requires string name")]
    MissingToolName,
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error(transparent)]
    Core(#[from] crate::CtxError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Return the MCP tool specifications supported by the engine.
#[must_use]
pub fn tool_specs() -> Value {
    Value::Array(vec![
        json!({
            "name": "file_search",
            "description": "Search allowed roots by path and/or file content.",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "pattern": { "type": "string" },
                    "mode": { "type": "string", "enum": ["path", "content", "both"], "default": "both" },
                    "regex": { "type": "boolean", "default": false },
                    "max_results": { "type": "integer", "default": 50 },
                    "context_lines": { "type": "integer", "default": 2 },
                    "max_content_files": { "type": "integer", "default": 2048 },
                    "max_content_bytes": { "type": "integer", "default": 67108864 },
                    "whole_word": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "read_file",
            "description": "Read a file from allowed roots with optional line range.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string" },
                    "start_line": { "type": "integer" },
                    "end_line": { "type": "integer" }
                }
            }
        }),
        json!({
            "name": "get_file_tree",
            "description": "Return a compact tree for allowed roots.",
            "inputSchema": {
                "type": "object",
                "properties": { "max_depth": { "type": "integer", "default": 3 } }
            }
        }),
        json!({
            "name": "get_code_structure",
            "description": "Return lightweight top-level code symbols for supported source files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional file or directory paths relative to an allowed root. Empty means whole catalog."
                    }
                }
            }
        }),
        json!({
            "name": "get_repo_map",
            "description": "Rank relevant repository files with deterministic personalized PageRank over codemap symbol references.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Optional literal query. Matching indexed files become personalized PageRank seeds."
                    },
                    "seed_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional explicit file or directory seed paths, relative to an allowed root or absolute."
                    },
                    "max_files": { "type": "integer", "default": 20 }
                }
            }
        }),
    ])
}

/// Dispatch one MCP `tools/call` params object and return the MCP tool response.
pub fn handle_tool_call<P>(provider: &P, params: &Value) -> Result<Value, DispatchError>
where
    P: CatalogProvider + Sync,
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
    P: CatalogProvider + Sync,
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
    let structured = match name {
        "file_search" => {
            let args: FileSearchArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response =
                search_snapshot_cancellable(provider, &snapshot, &args.into_request(), cancel)?;
            serde_json::to_value(response)?
        }
        "read_file" => {
            cancel.check_cancelled()?;
            let args: ReadFileArgs = serde_json::from_value(arguments)?;
            let response = read_file(provider, &args.into_request())?;
            cancel.check_cancelled()?;
            serde_json::to_value(response)?
        }
        "get_file_tree" => {
            let args: FileTreeArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            serde_json::to_value(get_file_tree(&snapshot, args.max_depth.unwrap_or(3)))?
        }
        "get_code_structure" => {
            let args: CodeStructureArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let response =
                get_code_structure(provider, &snapshot, &args.paths.unwrap_or_default())?;
            cancel.check_cancelled()?;
            serde_json::to_value(response)?
        }
        "get_repo_map" => {
            let args: RepoMapArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response =
                get_repo_map_cancellable(provider, &snapshot, &args.into_request(), cancel)?;
            serde_json::to_value(response)?
        }
        other => return Err(DispatchError::UnknownTool(other.to_string())),
    };

    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&structured)? }],
        "structuredContent": structured,
    }))
}

/// Decode one JSON tool-call params object and encode the tool response as JSON.
pub fn handle_tool_call_json<P>(provider: &P, request_json: &str) -> Result<String, DispatchError>
where
    P: CatalogProvider + Sync,
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
    P: CatalogProvider + Sync,
{
    let params: Value = serde_json::from_str(request_json)?;
    match handle_tool_call_cancellable(provider, &params, cancel) {
        Ok(response) => Ok(serde_json::to_string(&response)?),
        Err(err) if matches!(err, DispatchError::Core(CtxError::Cancelled)) => Ok(
            dispatch_error_json(dispatch_error_kind(&err), &err.to_string()),
        ),
        Err(err) => Err(err),
    }
}

#[must_use]
pub fn dispatch_error_kind(err: &DispatchError) -> &'static str {
    match err {
        DispatchError::MissingToolName => "missing_tool_name",
        DispatchError::UnknownTool(_) => "unknown_tool",
        DispatchError::Core(CtxError::Cancelled) => "cancelled",
        DispatchError::Core(_) => "core",
        DispatchError::Json(_) => "json",
    }
}

#[must_use]
pub fn dispatch_error_json(kind: &str, message: &str) -> String {
    json!({ "error": { "kind": kind, "message": message } }).to_string()
}

#[derive(Debug, Deserialize)]
struct FileSearchArgs {
    pattern: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default = "default_context_lines")]
    context_lines: usize,
    #[serde(default = "default_max_content_files")]
    max_content_files: usize,
    #[serde(default = "default_max_content_bytes")]
    max_content_bytes: u64,
    #[serde(default)]
    whole_word: bool,
}

impl FileSearchArgs {
    fn into_request(self) -> SearchRequest {
        SearchRequest {
            pattern: self.pattern,
            mode: match self.mode.as_str() {
                "path" => SearchMode::Path,
                "content" => SearchMode::Content,
                _ => SearchMode::Both,
            },
            regex: self.regex,
            max_results: self.max_results,
            context_lines: self.context_lines,
            max_content_files: self.max_content_files,
            max_content_bytes: self.max_content_bytes,
            whole_word: self.whole_word,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: PathBuf,
    start_line: Option<usize>,
    end_line: Option<usize>,
    limit: Option<usize>,
}

impl ReadFileArgs {
    fn into_request(self) -> ReadFileRequest {
        ReadFileRequest {
            path: self.path,
            start_line: self.start_line,
            end_line: self.end_line,
            limit: self.limit,
        }
    }
}

#[derive(Debug, Deserialize)]
struct FileTreeArgs {
    max_depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CodeStructureArgs {
    paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Deserialize)]
struct RepoMapArgs {
    query: Option<String>,
    #[serde(default)]
    seed_paths: Vec<PathBuf>,
    #[serde(default = "default_repo_map_max_files")]
    max_files: usize,
}

impl RepoMapArgs {
    fn into_request(self) -> RepoMapRequest {
        RepoMapRequest {
            query: self.query,
            seed_paths: self.seed_paths,
            max_files: self.max_files,
        }
    }
}

fn default_repo_map_max_files() -> usize {
    20
}

fn default_mode() -> String {
    "both".to_string()
}

fn default_max_results() -> usize {
    50
}

fn default_context_lines() -> usize {
    2
}

fn default_max_content_files() -> usize {
    2_048
}

fn default_max_content_bytes() -> u64 {
    64 * 1024 * 1024
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions};
    use std::fs;

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
}
