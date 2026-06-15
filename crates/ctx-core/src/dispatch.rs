//! Transport-neutral MCP tool dispatch for the context engine.

use crate::edit::{self, EditRequest, FileChange};
use crate::{
    CancelToken, CatalogProvider, CtxError, ReadFileRequest, RepoMapRequest, SearchMode,
    SearchRequest, SingletonWorkspaceResolver, WorkspaceResolver, build_context_cancellable,
    get_code_structure, get_file_tree, get_repo_map_cancellable, manage_selection, read_file,
    search_snapshot_cancellable, workspace_context,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

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
    #[error(transparent)]
    Edit(#[from] edit::EditError),
}

/// Return the MCP tool specifications supported by the engine.
#[must_use]
pub fn tool_specs() -> Value {
    let tools = vec![
        json!({
            "name": "edit",
            "description": "Edit an existing file in one of four modes. For hashline, first call read_file with view=\"hashline\" to get the [PATH#TAG] header and line numbers.",
            "inputSchema": {
                "type": "object",
                "required": ["mode"],
                "properties": {
                    "workspace": workspace_schema(),
                    "mode": { "type": "string", "enum": ["replace", "patch", "apply_patch", "hashline"], "description": "replace: fuzzy string replace (path+edits). patch: anchored diff hunks (path+entries). apply_patch: Codex '*** Begin Patch' envelope (patch). hashline: [PATH#TAG] line-anchored ops (patch)." },
                    "path": { "type": "string", "description": "Target file for replace/patch modes." },
                    "edits": { "type": "array", "description": "replace mode.", "items": { "type": "object", "required": ["old_text", "new_text"], "properties": { "old_text": {"type": "string"}, "new_text": {"type": "string"}, "all": {"type": "boolean"} } } },
                    "entries": { "type": "array", "description": "patch mode: {op: update|create|delete, diff?, rename?}.", "items": { "type": "object" } },
                    "patch": { "type": "string", "description": "Full patch text for apply_patch / hashline modes." }
                }
            }
        }),
        json!({
            "name": "write",
            "description": "Create or overwrite a file with exact content (within allowed roots).",
            "inputSchema": { "type": "object", "required": ["path", "content"], "properties": { "workspace": workspace_schema(), "path": {"type": "string"}, "content": {"type": "string"} } }
        }),
        json!({
            "name": "delete",
            "description": "Delete a file (within allowed roots).",
            "inputSchema": { "type": "object", "required": ["path"], "properties": { "workspace": workspace_schema(), "path": {"type": "string"} } }
        }),
        json!({
            "name": "move",
            "description": "Move or rename a file (within allowed roots).",
            "inputSchema": { "type": "object", "required": ["from", "to"], "properties": { "workspace": workspace_schema(), "from": {"type": "string"}, "to": {"type": "string"} } }
        }),
        json!({
            "name": "build_context",
            "description": "Build a deterministic query-focused context within a token budget.",
            "inputSchema": {
                "type": "object",
                "required": ["query", "token_budget"],
                "properties": {
                    "workspace": workspace_schema(),
                    "query": {
                        "type": "string",
                        "description": "Search query used for file ranking and repo-map personalization."
                    },
                    "token_budget": {
                        "type": "integer",
                        "description": "Maximum assembled context tokens."
                    },
                    "max_files": {
                        "type": "integer",
                        "default": 20,
                        "description": "Maximum number of files to include."
                    }
                }
            }
        }),
        json!({
            "name": "workspace_context",
            "description": "Assemble the current persistent selection into context text with token breakdowns.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": workspace_schema(),
                    "include": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["file-map", "contents", "tokens"] },
                        "description": "Optional text sections to include. Empty means file-map and contents."
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Optional notes/instructions to include in the context snapshot."
                    }
                }
            }
        }),
        json!({
            "name": "manage_selection",
            "description": "Persist and summarize the selected file set with token estimates.",
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "workspace": workspace_schema(),
                    "op": { "type": "string", "enum": ["get", "add", "remove", "set", "clear"] },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File or directory paths relative to an allowed root, or in-root absolute paths."
                    },
                    "mode": { "type": "string", "enum": ["full", "slices", "codemap_only"], "default": "full" },
                    "slices": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["path"],
                            "properties": {
                                "path": { "type": "string" },
                                "ranges": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "required": ["start_line", "end_line"],
                                        "properties": {
                                            "start_line": { "type": "integer" },
                                            "end_line": { "type": "integer" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
        json!({
            "name": "file_search",
            "description": "Search allowed roots by path and/or file content.",
            "inputSchema": {
                "type": "object",
                "required": ["pattern"],
                "properties": {
                    "workspace": workspace_schema(),
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
                    "workspace": workspace_schema(),
                    "path": { "type": "string" },
                    "start_line": { "type": "integer", "description": "1-based line to start from (alias: offset)." },
                    "end_line": { "type": "integer", "description": "1-based inclusive end line." },
                    "limit": { "type": "integer", "description": "Max lines to return from start_line; overrides end_line." },
                    "view": { "type": "string", "enum": ["raw", "hashline"], "description": "hashline: return the whole file as a [PATH#TAG] header + 1-based N:LINE rows, for authoring hashline edits." }
                }
            }
        }),
        json!({
            "name": "get_file_tree",
            "description": "Return a compact ASCII directory tree for allowed roots. `auto` mode (default) adapts depth/breadth to a size budget; pass `path` to scope to a subdirectory on large repos.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": workspace_schema(),
                    "mode": { "type": "string", "enum": ["auto", "full", "folders"], "default": "auto", "description": "auto: fit a size budget (degrades depth->folders->top-level, with a note). full: everything (can be large). folders: directories only." },
                    "max_depth": { "type": "integer", "description": "Maximum depth (root = 0)." },
                    "path": { "type": "string", "description": "Scope the tree to this subdirectory (relative to a root)." }
                }
            }
        }),
        json!({
            "name": "get_code_structure",
            "description": "Return code symbols (kind/name/line, including nested definitions like methods) for supported source files. Parsed with tree-sitter: Rust, Python, JavaScript, TypeScript, Go, Java, C, C++, C#, Ruby, PHP.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": workspace_schema(),
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
                    "workspace": workspace_schema(),
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
    ];
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut tools = tools;
        tools.push(json!({
            "name": "manage_workspaces",
            "description": "List, add, remove, or inspect registered filesystem workspaces.",
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "op": { "type": "string", "enum": ["list", "add", "remove", "get"] },
                    "name": {
                        "type": "string",
                        "description": "Workspace name for add, remove, or get."
                    },
                    "roots": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Allowed roots for add. Empty roots are fail-closed."
                    }
                }
            }
        }));
        Value::Array(tools)
    }
    #[cfg(target_arch = "wasm32")]
    {
        Value::Array(tools)
    }
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
        let structured = serde_json::to_value(resolver.manage_workspaces(args)?)?;
        return tool_response(structured);
    }
    let workspace = workspace_arg(&arguments)?;
    let provider = resolver.resolve_workspace(workspace)?;
    let provider = &*provider;
    let structured = match name {
        "manage_selection" => {
            let args: crate::ManageSelectionRequest = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let response = manage_selection(provider, &snapshot, &args)?;
            cancel.check_cancelled()?;
            serde_json::to_value(response)?
        }
        "workspace_context" => {
            let args: crate::WorkspaceContextRequest = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let response = workspace_context(provider, &snapshot, &args)?;
            cancel.check_cancelled()?;
            return tool_response_text(&response);
        }
        "build_context" => {
            let args: crate::BuildContextRequest = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let response = build_context_cancellable(provider, &snapshot, &args, cancel)?;
            cancel.check_cancelled()?;
            return tool_response_text(&response);
        }
        "file_search" => {
            let args: FileSearchArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response =
                search_snapshot_cancellable(provider, &snapshot, &args.into_request(), cancel)?;
            return tool_response_text(&response);
        }
        "read_file" => {
            cancel.check_cancelled()?;
            let args: ReadFileArgs = serde_json::from_value(arguments)?;
            let hashline = args.view.as_deref() == Some("hashline");
            let mut request = args.into_request();
            if hashline {
                // Whole-file view: line numbers are absolute and the tag covers
                // the entire file the model anchors hashline edits against.
                request.start_line = None;
                request.end_line = None;
                request.limit = None;
            }
            let response = read_file(provider, &request)?;
            cancel.check_cancelled()?;
            if hashline {
                let view = edit::hashline_view(&response.display_path, &response.content);
                return Ok(json!({
                    "content": [{ "type": "text", "text": view }],
                    "structuredContent": {
                        "path": response.display_path,
                        "hashline_tag": edit::snapshot_tag(&response.content),
                        "total_lines": response.total_lines,
                    },
                }));
            }
            return tool_response_text(&response);
        }
        "edit" => {
            cancel.check_cancelled()?;
            let args: EditArgs = serde_json::from_value(arguments)?;
            let request = args.into_request()?;
            let changes = edit::apply(&request, &ProviderReader { provider })?;
            cancel.check_cancelled()?;
            return tool_response_text(&apply_changes(provider, changes)?);
        }
        "write" => {
            let args: WriteArgs = serde_json::from_value(arguments)?;
            provider.write_text(Path::new(&args.path), &args.content)?;
            return tool_response_text(&EditResult {
                files: vec![EditedFile::with_content(
                    "write",
                    args.path,
                    None,
                    &args.content,
                )],
            });
        }
        "delete" => {
            let args: DeleteArgs = serde_json::from_value(arguments)?;
            provider.delete_file(Path::new(&args.path))?;
            return tool_response_text(&EditResult {
                files: vec![EditedFile {
                    action: "delete",
                    path: args.path,
                    moved_to: None,
                    tag: None,
                    view: None,
                    diagnostics: Vec::new(),
                }],
            });
        }
        "move" => {
            let args: MoveArgs = serde_json::from_value(arguments)?;
            provider.rename_file(Path::new(&args.from), Path::new(&args.to))?;
            return tool_response_text(&EditResult {
                files: vec![EditedFile {
                    action: "move",
                    path: args.from,
                    moved_to: Some(args.to),
                    tag: None,
                    view: None,
                    diagnostics: Vec::new(),
                }],
            });
        }
        "get_file_tree" => {
            let args: FileTreeArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let options = crate::FileTreeOptions {
                mode: crate::TreeMode::from_arg(args.mode.as_deref()),
                max_depth: args.max_depth,
                path: args.path,
            };
            let response = get_file_tree(&snapshot, &options);
            return tool_response_text(&response);
        }
        "get_code_structure" => {
            let args: CodeStructureArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let response =
                get_code_structure(provider, &snapshot, &args.paths.unwrap_or_default())?;
            cancel.check_cancelled()?;
            return tool_response_text(&response);
        }
        "get_repo_map" => {
            let args: RepoMapArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response =
                get_repo_map_cancellable(provider, &snapshot, &args.into_request(), cancel)?;
            return tool_response_text(&response);
        }
        other => return Err(DispatchError::UnknownTool(other.to_string())),
    };

    tool_response(structured)
}

fn tool_response(structured: Value) -> Result<Value, DispatchError> {
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&structured)? }],
        "structuredContent": structured,
    }))
}

/// Wrap a tool response so the model-facing `content[].text` is a compact,
/// readable rendering while the full data stays in `structuredContent`. This
/// avoids dumping verbose JSON (escaped newlines, repeated keys) at the model.
fn tool_response_text<T>(response: &T) -> Result<Value, DispatchError>
where
    T: serde::Serialize + ToolText,
{
    let text = response.tool_text();
    let structured = serde_json::to_value(response)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    }))
}

/// Compact text rendering used for a tool's `content[].text`.
trait ToolText {
    fn tool_text(&self) -> String;
}

impl ToolText for crate::ReadFileResponse {
    fn tool_text(&self) -> String {
        self.content.clone()
    }
}

impl ToolText for crate::FileTreeResponse {
    fn tool_text(&self) -> String {
        let note = self.note.as_deref().filter(|n| !n.is_empty());
        match (self.tree.is_empty(), note) {
            (false, Some(note)) => format!("{}\n\n(note: {note})", self.tree),
            (true, Some(note)) => format!("(note: {note})"),
            (_, None) => self.tree.clone(),
        }
    }
}

impl ToolText for crate::WorkspaceContextResponse {
    fn tool_text(&self) -> String {
        self.context.clone()
    }
}

impl ToolText for crate::BuildContextResponse {
    fn tool_text(&self) -> String {
        self.context.clone()
    }
}

impl ToolText for crate::SearchResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        if !self.path_matches.is_empty() {
            out.push_str("path matches:\n");
            for m in &self.path_matches {
                out.push_str("  ");
                out.push_str(&m.display_path);
                out.push('\n');
            }
        }
        if !self.content_matches.is_empty() {
            out.push_str("content matches:\n");
            for m in &self.content_matches {
                out.push_str(&format!(
                    "  {}:{}:{}: {}\n",
                    m.display_path,
                    m.line,
                    m.column,
                    m.text.trim_end()
                ));
            }
        }
        if self.path_matches.is_empty() && self.content_matches.is_empty() {
            out.push_str("(no matches)\n");
        }
        let totals = &self.totals;
        out.push_str(&format!(
            "totals: {} path, {} content, {} files scanned{}\n",
            totals.path_matches,
            totals.content_matches,
            totals.scanned_files,
            if totals.totals_are_lower_bound {
                " (lower bound)"
            } else {
                ""
            }
        ));
        out
    }
}

impl ToolText for crate::repomap::RepoMapResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            out.push_str(&file.score);
            out.push('\t');
            out.push_str(&file.display_path);
            let names: Vec<&str> = file
                .symbols
                .iter()
                .take(8)
                .map(|s| s.name.as_str())
                .collect();
            if !names.is_empty() {
                out.push('\t');
                out.push_str(&names.join(", "));
                if file.symbols.len() > names.len() {
                    out.push_str(", …");
                }
            }
            out.push('\n');
        }
        if self.files.is_empty() {
            out.push_str("(no ranked files)\n");
        }
        if !self.diagnostics.is_empty() {
            out.push_str(&format!(
                "({} files skipped; parse diagnostics in structuredContent)\n",
                self.diagnostics.len()
            ));
        }
        out
    }
}

impl ToolText for crate::codemap::CodeStructureResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            out.push_str(&file.path);
            out.push('\n');
            for symbol in &file.symbols {
                match &symbol.signature {
                    Some(signature) => out.push_str(&format!(
                        "  {} ({}): {}\n",
                        symbol.kind, symbol.line, signature
                    )),
                    None => out.push_str(&format!(
                        "  {} {} ({})\n",
                        symbol.kind, symbol.name, symbol.line
                    )),
                }
                for member in &symbol.members {
                    match &member.signature {
                        Some(signature) => out.push_str(&format!("    - {signature}\n")),
                        None => out.push_str(&format!("    - {}\n", member.name)),
                    }
                }
            }
        }
        if self.files.is_empty() {
            out.push_str("(no symbols)\n");
        }
        if self.omitted > 0 {
            out.push_str(&format!(
                "({} files omitted: unsupported or no symbols)\n",
                self.omitted
            ));
        }
        if !self.diagnostics.is_empty() {
            out.push_str(&format!(
                "({} parse diagnostics in structuredContent)\n",
                self.diagnostics.len()
            ));
        }
        out
    }
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

#[must_use]
pub fn dispatch_error_kind(err: &DispatchError) -> &'static str {
    match err {
        DispatchError::MissingToolName => "missing_tool_name",
        DispatchError::UnknownTool(_) => "unknown_tool",
        DispatchError::Core(CtxError::Cancelled) => "cancelled",
        DispatchError::Core(
            CtxError::AmbiguousWorkspace
            | CtxError::UnknownWorkspace(_)
            | CtxError::ManageWorkspacesUnsupported
            | CtxError::MissingWorkspaceName,
        ) => "workspace",
        DispatchError::Core(_) => "core",
        DispatchError::Json(_) => "json",
        DispatchError::Edit(_) => "edit",
    }
}

#[must_use]
pub fn dispatch_error_json(kind: &str, message: &str) -> String {
    json!({ "error": { "kind": kind, "message": message } }).to_string()
}

#[must_use]
fn workspace_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional workspace id to route this tool call. Required when multiple workspaces are registered."
    })
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

#[derive(Debug, Deserialize)]
struct FileSearchArgs {
    pattern: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_max_results", deserialize_with = "lenient_usize")]
    max_results: usize,
    #[serde(default = "default_context_lines", deserialize_with = "lenient_usize")]
    context_lines: usize,
    #[serde(
        default = "default_max_content_files",
        deserialize_with = "lenient_usize"
    )]
    max_content_files: usize,
    #[serde(
        default = "default_max_content_bytes",
        deserialize_with = "lenient_u64"
    )]
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
    #[serde(default, alias = "offset", deserialize_with = "lenient_opt_usize")]
    start_line: Option<usize>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    end_line: Option<usize>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    limit: Option<usize>,
    #[serde(default)]
    view: Option<String>,
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
struct EditArgs {
    mode: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    edits: Vec<edit::ReplaceEdit>,
    #[serde(default)]
    entries: Vec<edit::PatchEntry>,
    #[serde(default)]
    patch: Option<String>,
}

impl EditArgs {
    fn into_request(self) -> Result<EditRequest, DispatchError> {
        let EditArgs {
            mode,
            path,
            edits,
            entries,
            patch,
        } = self;
        let err = |detail: String| {
            DispatchError::Edit(edit::EditError::Parse {
                mode: "edit",
                detail,
            })
        };
        match mode.as_str() {
            "replace" => Ok(EditRequest::Replace {
                path: path.ok_or_else(|| err("mode `replace` requires `path`".to_string()))?,
                edits,
            }),
            "patch" => Ok(EditRequest::Patch {
                path: path.ok_or_else(|| err("mode `patch` requires `path`".to_string()))?,
                entries,
            }),
            "apply_patch" | "apply-patch" => Ok(EditRequest::ApplyPatch {
                patch: patch
                    .ok_or_else(|| err("mode `apply_patch` requires `patch`".to_string()))?,
            }),
            "hashline" => Ok(EditRequest::Hashline {
                patch: patch.ok_or_else(|| err("mode `hashline` requires `patch`".to_string()))?,
            }),
            other => Err(err(format!("unknown edit mode: {other}"))),
        }
    }
}

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct DeleteArgs {
    path: String,
}

#[derive(Debug, Deserialize)]
struct MoveArgs {
    from: String,
    to: String,
}

/// Adapts a [`CatalogProvider`] into an [`edit::FileReader`]; reads are
/// containment-checked by the provider's root policy.
struct ProviderReader<'a, P: CatalogProvider + ?Sized> {
    provider: &'a P,
}

impl<P: CatalogProvider + ?Sized> edit::FileReader for ProviderReader<'_, P> {
    fn read_text(&self, path: &str) -> Option<String> {
        self.provider
            .read_bytes(Path::new(path))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[derive(serde::Serialize)]
struct EditedFile {
    action: &'static str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    moved_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    view: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<crate::codemap::SyntaxIssue>,
}

impl EditedFile {
    fn with_content(
        action: &'static str,
        path: String,
        moved_to: Option<String>,
        content: &str,
    ) -> Self {
        let display = moved_to.clone().unwrap_or_else(|| path.clone());
        Self {
            action,
            tag: Some(edit::snapshot_tag(content)),
            view: Some(edit::hashline_view(&display, content)),
            diagnostics: crate::codemap::syntax_diagnostics(&display, content),
            path,
            moved_to,
        }
    }
}

#[derive(serde::Serialize)]
struct EditResult {
    files: Vec<EditedFile>,
}

impl ToolText for EditResult {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            match &file.moved_to {
                Some(to) => out.push_str(&format!("{} {} -> {}\n", file.action, file.path, to)),
                None => out.push_str(&format!("{} {}\n", file.action, file.path)),
            }
        }
        for file in &self.files {
            for issue in &file.diagnostics {
                out.push_str(&format!(
                    "  \u{26a0} {} line {}: {}\n",
                    file.path, issue.line, issue.message
                ));
            }
        }
        for file in &self.files {
            if let Some(view) = &file.view {
                out.push('\n');
                out.push_str(view);
            }
        }
        out
    }
}

fn apply_changes<P: CatalogProvider + ?Sized>(
    provider: &P,
    changes: Vec<FileChange>,
) -> Result<EditResult, DispatchError> {
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        let edited = match change {
            FileChange::Create { path, content } => {
                provider.write_text(Path::new(&path), &content)?;
                EditedFile::with_content("create", path, None, &content)
            }
            FileChange::Update { path, content } => {
                provider.write_text(Path::new(&path), &content)?;
                EditedFile::with_content("update", path, None, &content)
            }
            FileChange::Delete { path } => {
                provider.delete_file(Path::new(&path))?;
                EditedFile {
                    action: "delete",
                    path,
                    moved_to: None,
                    tag: None,
                    view: None,
                    diagnostics: Vec::new(),
                }
            }
            FileChange::Rename { from, to, content } => {
                provider.rename_file(Path::new(&from), Path::new(&to))?;
                provider.write_text(Path::new(&to), &content)?;
                EditedFile::with_content("rename", from, Some(to), &content)
            }
        };
        files.push(edited);
    }
    Ok(EditResult { files })
}

#[derive(Debug, Deserialize)]
struct FileTreeArgs {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    max_depth: Option<usize>,
    #[serde(default)]
    path: Option<String>,
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
    #[serde(
        default = "default_repo_map_max_files",
        deserialize_with = "lenient_usize"
    )]
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

/// Coerce a JSON value into u64, accepting integers and integer-valued strings.
/// LLM clients frequently emit numbers as strings (e.g. "130"); be forgiving.
fn coerce_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => {
            let trimmed = s.trim();
            trimmed.parse::<u64>().ok().or_else(|| {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                    .map(|f| f as u64)
            })
        }
        _ => None,
    }
}

fn coerce_usize(value: &Value) -> Option<usize> {
    coerce_u64(value).and_then(|n| usize::try_from(n).ok())
}

/// Deserialize a usize, accepting an integer or an integer-valued string.
fn lenient_usize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
    let value = Value::deserialize(deserializer)?;
    coerce_usize(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got {value}")))
}

/// Deserialize a u64, accepting an integer or an integer-valued string.
fn lenient_u64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    coerce_u64(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got {value}")))
}

/// Deserialize an Option<usize>, accepting null, an integer, or an integer string.
fn lenient_opt_usize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    coerce_usize(&value)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got {value}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions, WorkspaceRegistry};
    use std::{fs, sync::Arc};

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
            }],
            diagnostics: vec![],
            omitted: 0,
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
        assert!(response["structuredContent"]["roots"].is_array());
    }

    fn provider_for(path: &std::path::Path) -> FsCatalogProvider {
        FsCatalogProvider::new(
            RootPolicy::new(vec![path.to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        )
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
        let structured = &response["structuredContent"];
        assert!(
            structured["context"]
                .as_str()
                .expect("context")
                .contains("<file_map>")
        );
        assert!(
            structured["context"]
                .as_str()
                .expect("context")
                .contains("<tokens>")
        );
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
