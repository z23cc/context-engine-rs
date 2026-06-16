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

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
pub trait DispatchProvider: CatalogProvider + Clone + Send + Sync + 'static {}
#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl<T> DispatchProvider for T where T: CatalogProvider + Clone + Send + Sync + 'static {}

#[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
pub trait DispatchProvider: CatalogProvider + Sync {}
#[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
impl<T> DispatchProvider for T where T: CatalogProvider + Sync {}

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
    let mut tools = vec![
        json!({
            "name": "edit",
            "description": "Edit an existing file in one of four modes. Required fields depend on mode: replace -> path + edits; patch -> path + entries; apply_patch -> patch; hashline -> patch. For hashline, first call read_file with view=\"hashline\" to get the [PATH#TAG] header and line numbers.",
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
            "name": "ast_search",
            "description": "Structural code search via a tree-sitter query over files of one language. The query MUST capture the matched region as @match; other captures (@x) come back as metavariables.",
            "inputSchema": {
                "type": "object",
                "required": ["query", "language"],
                "properties": {
                    "workspace": workspace_schema(),
                    "query": { "type": "string", "description": "Tree-sitter S-expression query, e.g. (call_expression function: (identifier) @name) @match" },
                    "language": { "type": "string", "enum": ["rust", "python", "javascript", "typescript", "tsx", "go", "java", "c", "cpp", "csharp", "ruby", "php"] },
                    "paths": { "type": "array", "items": {"type": "string"}, "description": "Optional file/dir scope relative to a root. Empty = whole catalog." },
                    "max_results": { "type": "integer", "description": "Cap on returned matches (default 100)." }
                }
            }
        }),
        json!({
            "name": "ast_edit",
            "description": "Structural rewrite of one file: replace every @match region of a tree-sitter query with a template, where ${name} is substituted by capture @name. No write if nothing matches.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "query", "replacement"],
                "properties": {
                    "workspace": workspace_schema(),
                    "path": { "type": "string" },
                    "query": { "type": "string", "description": "Tree-sitter query; MUST capture the region to replace as @match." },
                    "replacement": { "type": "string", "description": "Template; ${name} -> capture @name's text." }
                }
            }
        }),
        json!({
            "name": "git",
            "description": "Read-only git in the workspace root: op = status | diff | log | blame | show. diff takes optional path + staged; log takes count + path; blame takes path (+ lines L-range); show takes ref.",
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "workspace": workspace_schema(),
                    "op": { "type": "string", "enum": ["status", "diff", "log", "blame", "show"] },
                    "path": { "type": "string" },
                    "staged": { "type": "boolean", "description": "diff: staged vs HEAD." },
                    "ref": { "type": "string", "description": "show: commit/ref." },
                    "count": { "type": "integer", "description": "log: number of commits (default 20)." },
                    "lines": { "type": "string", "description": "blame: line range, e.g. 10,20." }
                }
            }
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
                    },
                    "seed_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional files to seed the repo-map PageRank, biasing selection toward them and their references."
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
            "description": "Search allowed roots by path and/or file content. Narrow scope with `extensions`/`include`/`exclude` globs; use `output_mode` to get just matching files or counts; `context_before`/`context_after` override symmetric `context_lines`.",
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
                    "context_before": { "type": "integer", "description": "Lines of context before each match (overrides context_lines)." },
                    "context_after": { "type": "integer", "description": "Lines of context after each match (overrides context_lines)." },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "default": "content", "description": "content: full matches; files_with_matches: matching files only; count: per-file counts." },
                    "extensions": { "type": "array", "items": { "type": "string" }, "description": "Extension whitelist without dot, e.g. [\"rs\",\"ts\"]." },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "Glob whitelist on path, e.g. [\"src/**\",\"*.rs\"]." },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "Glob blacklist on path, e.g. [\"**/target/**\"]." },
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
        json!({
            "name": "goto_definition",
            "description": "Find where a symbol is defined across the workspace (syntactic tree-sitter name match over 11 languages; deterministic, no language server). Returns each definition's path, line, kind, and signature. Not a scope/type resolver: results may include unrelated same-name symbols.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "max_results": { "type": "integer", "default": 200 }
                }
            }
        }),
        json!({
            "name": "call_hierarchy",
            "description": "Build a name-based call hierarchy for a symbol (deterministic, tree-sitter; no language server). incoming = callers (each reference mapped to its enclosing definition); outgoing = callees (the symbol body's references resolved to definitions). Best-effort: name-based, not a scope/type resolver.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "direction": { "type": "string", "enum": ["incoming", "outgoing", "both"], "default": "both" },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "max_results": { "type": "integer", "default": 200 }
                }
            }
        }),
        json!({
            "name": "find_references",
            "description": "Find references to a symbol across the workspace (syntactic tree-sitter name match over 11 languages; deterministic, no language server). Returns each reference's path, line, and kind. Set include_definitions to also return the symbol's definitions. Not a scope/type resolver: results may include unrelated same-name symbols and miss aliases/re-exports.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "include_definitions": { "type": "boolean", "default": false, "description": "Also return the symbol's definitions." },
                    "confident_only": { "type": "boolean", "default": false, "description": "Drop low-confidence (ambiguous name-only) references." },
                    "max_results": { "type": "integer", "default": 200 }
                }
            }
        }),
    ];
    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    tools.push(json!({
        "name": "semantic_search",
        "description": "Intent-based code retrieval over a persistent semantic index. Cold dense builds run in the background; queries return BM25-only results with a warming diagnostic until dense search is ready.",
        "inputSchema": {
            "type": "object",
            "required": ["query"],
            "properties": {
                "workspace": workspace_schema(),
                "query": { "type": "string", "description": "Natural-language or code intent query." },
                "mode": { "type": "string", "enum": ["hybrid", "semantic"], "default": "hybrid", "description": "hybrid: dense ANN + chunk BM25; semantic: dense ANN only." },
                "max_results": { "type": "integer", "default": 20 },
                "rerank": { "type": "boolean", "default": true }
            }
        }
    }));
    #[cfg(not(target_arch = "wasm32"))]
    {
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
            let args: BuildContextArgs = serde_json::from_value(arguments)?;
            let request = args.into_request();
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            cancel.check_cancelled()?;
            let response = build_context_cancellable(provider, &snapshot, &request, cancel)?;
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
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        "semantic_search" => {
            let args: SemanticSearchArgs = serde_json::from_value(arguments)?;
            let request = args.into_request();
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let index = provider
                .semantic_index()
                .ok_or(CtxError::SemanticUnavailable)?;
            let response =
                index.search_background((*provider).clone(), snapshot, &request, cancel)?;
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
            let old = read_old(provider, &args.path);
            provider.write_text(Path::new(&args.path), &args.content)?;
            return tool_response_text(&EditResult {
                files: vec![EditedFile::with_content(
                    "write",
                    args.path,
                    None,
                    &args.content,
                    &old,
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
                    diff: None,
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
                    diff: None,
                    diagnostics: Vec::new(),
                }],
            });
        }
        "ast_search" => {
            let args: AstSearchArgs = serde_json::from_value(arguments)?;
            if !crate::codemap::ast_language_supported(&args.language) {
                return Err(DispatchError::Edit(edit::EditError::Parse {
                    mode: "ast_search",
                    detail: format!("unsupported language: {}", args.language),
                }));
            }
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let mut matches: Vec<AstFileMatch> = Vec::new();
            let mut files_scanned = 0usize;
            for entry in &snapshot.entries {
                if crate::codemap::path_language_name(&entry.rel_path)
                    != Some(args.language.as_str())
                {
                    continue;
                }
                if !args.paths.is_empty()
                    && !args
                        .paths
                        .iter()
                        .any(|scope| path_in_scope(&entry.rel_path, scope))
                {
                    continue;
                }
                let Ok(bytes) = provider.read_bytes(&entry.abs_path) else {
                    continue;
                };
                files_scanned += 1;
                let source = String::from_utf8_lossy(&bytes);
                let found = crate::codemap::ast_search(
                    &entry.rel_path,
                    &source,
                    &args.query,
                    args.max_results,
                )
                .map_err(|detail| {
                    DispatchError::Edit(edit::EditError::Parse {
                        mode: "ast_search",
                        detail,
                    })
                })?;
                for item in found {
                    matches.push(AstFileMatch {
                        path: entry.rel_path.clone(),
                        line: item.line,
                        text: item.text,
                        captures: item.captures,
                    });
                    if matches.len() >= args.max_results {
                        break;
                    }
                }
                if matches.len() >= args.max_results {
                    break;
                }
                cancel.check_cancelled()?;
            }
            return tool_response_text(&AstSearchResponse {
                matches,
                files_scanned,
            });
        }
        "ast_edit" => {
            let args: AstEditArgs = serde_json::from_value(arguments)?;
            let bytes = provider.read_bytes(Path::new(&args.path))?;
            let source = String::from_utf8_lossy(&bytes).into_owned();
            let (rewritten, count) =
                crate::codemap::ast_rewrite(&args.path, &source, &args.query, &args.replacement)
                    .map_err(|detail| {
                        DispatchError::Edit(edit::EditError::Parse {
                            mode: "ast_edit",
                            detail,
                        })
                    })?;
            if count == 0 {
                return Ok(json!({
                    "content": [{ "type": "text", "text": format!("ast_edit: no matches in {}\n", args.path) }],
                    "structuredContent": { "path": args.path, "rewrites": 0 },
                }));
            }
            provider.write_text(Path::new(&args.path), &rewritten)?;
            return tool_response_text(&EditResult {
                files: vec![EditedFile::with_content(
                    "ast_edit", args.path, None, &rewritten, &source,
                )],
            });
        }
        "git" => {
            let args: GitArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let root = snapshot
                .roots
                .first()
                .map(|root| root.path.clone())
                .ok_or(DispatchError::Core(CtxError::NoRoots))?;
            let output = run_git(&root, &args)?;
            return Ok(json!({
                "content": [{ "type": "text", "text": output.clone() }],
                "structuredContent": { "op": args.op, "output": output },
            }));
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
        "goto_definition" => {
            let args: NavigateArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response = crate::navigate::goto_definition_cancellable(
                provider,
                &snapshot,
                &args.into_request(),
                cancel,
            )?;
            return tool_response_text(&response);
        }
        "find_references" => {
            let args: NavigateArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response = crate::navigate::find_references_cancellable(
                provider,
                &snapshot,
                &args.into_request(),
                cancel,
            )?;
            return tool_response_text(&response);
        }
        "call_hierarchy" => {
            let args: CallHierarchyArgs = serde_json::from_value(arguments)?;
            let snapshot = provider.snapshot_arc_cancellable(cancel)?;
            let response = crate::navigate::call_hierarchy_cancellable(
                provider,
                &snapshot,
                &args.into_request(),
                cancel,
            )?;
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

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl ToolText for crate::SemanticSearchResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        if self.results.is_empty() {
            out.push_str("(no semantic matches)\n");
        } else {
            out.push_str("semantic matches:\n");
            for result in &self.results {
                let symbol = result
                    .symbol
                    .as_deref()
                    .map(|symbol| format!(" {symbol}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {}:{}-{}{} (score {:.4})\n",
                    result.display_path, result.line_start, result.line_end, symbol, result.score
                ));
            }
        }
        out.push_str(&format!(
            "totals: {} chunks, {} dense, {} bm25, {} reranked\n",
            self.totals.chunks,
            self.totals.dense_candidates,
            self.totals.bm25_candidates,
            self.totals.reranked
        ));
        if !self.diagnostics.is_empty() {
            out.push_str(&format!(
                "({} diagnostics in structuredContent)\n",
                self.diagnostics.len()
            ));
        }
        out
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
        if !self.match_files.is_empty() {
            out.push_str("matching files:\n");
            for f in &self.match_files {
                out.push_str(&format!("  {} ({})\n", f.display_path, f.count));
            }
        }
        if self.path_matches.is_empty()
            && self.content_matches.is_empty()
            && self.match_files.is_empty()
        {
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

/// Character budget for the rendered repo-map text (~3k tokens). Like aider's
/// `map_tokens`, the map degrades to fit: top-ranked files render first and the
/// tail is dropped with a note, so a large `max_files` on a big repo never blows
/// up the model-facing text. Full ranking stays in `structuredContent`.
const REPO_MAP_TEXT_BUDGET_CHARS: usize = 12_000;
const REPO_MAP_MAX_NAMES: usize = 8;

fn render_repo_map_text(response: &crate::repomap::RepoMapResponse, budget: usize) -> String {
    let mut out = String::new();
    let mut rendered = 0usize;
    for file in &response.files {
        let mut line = String::new();
        line.push_str(&file.score);
        line.push('\t');
        line.push_str(&file.display_path);
        let names: Vec<&str> = file
            .symbols
            .iter()
            .take(REPO_MAP_MAX_NAMES)
            .map(|s| s.name.as_str())
            .collect();
        if !names.is_empty() {
            line.push('\t');
            line.push_str(&names.join(", "));
            if file.symbols.len() > names.len() {
                line.push_str(", …");
            }
        }
        line.push('\n');
        // Always emit at least the top-ranked file; stop once the budget is hit.
        if rendered > 0 && out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
        rendered += 1;
    }
    if response.files.is_empty() {
        out.push_str("(no ranked files)\n");
    }
    let omitted = response.files.len() - rendered;
    if omitted > 0 {
        out.push_str(&format!(
            "(+{omitted} more ranked files omitted to fit the map budget; full ranking in structuredContent)\n"
        ));
    }
    if !response.diagnostics.is_empty() {
        out.push_str(&format!(
            "({} files skipped; parse diagnostics in structuredContent)\n",
            response.diagnostics.len()
        ));
    }
    out
}

impl ToolText for crate::repomap::RepoMapResponse {
    fn tool_text(&self) -> String {
        render_repo_map_text(self, REPO_MAP_TEXT_BUDGET_CHARS)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ToolText for crate::workspace::ManageWorkspacesResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for ws in &self.workspaces {
            let roots: Vec<String> = ws.roots.iter().map(|r| r.display().to_string()).collect();
            out.push_str(&format!("{}\t{}\n", ws.name, roots.join(", ")));
        }
        if self.workspaces.is_empty() {
            out.push_str("(no workspaces)\n");
        }
        if let Some(changed) = &self.changed {
            out.push_str(&format!("changed: {changed}\n"));
        }
        out
    }
}

impl ToolText for crate::navigate::DefinitionResponse {
    fn tool_text(&self) -> String {
        if self.definitions.is_empty() {
            return format!("(no definitions for {})\n", self.symbol);
        }
        let mut out = String::new();
        for def in &self.definitions {
            match &def.signature {
                Some(sig) => out.push_str(&format!(
                    "{}:{} {} {}\n",
                    def.display_path, def.line, def.kind, sig
                )),
                None => out.push_str(&format!("{}:{} {}\n", def.display_path, def.line, def.kind)),
            }
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.definitions.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::ReferencesResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        if !self.definitions.is_empty() {
            out.push_str("definitions:\n");
            for def in &self.definitions {
                out.push_str(&format!(
                    "  {}:{} {}\n",
                    def.display_path, def.line, def.kind
                ));
            }
        }
        if self.references.is_empty() {
            out.push_str(&format!("(no references to {})\n", self.symbol));
            return out;
        }
        if self.definition_count > 1 {
            out.push_str(&format!(
                "references ({} definitions of this name \u{2014} low-confidence may be unrelated):\n",
                self.definition_count
            ));
        } else {
            out.push_str("references:\n");
        }
        for r in &self.references {
            let mark = match r.confidence {
                crate::navigate::Confidence::High => "",
                crate::navigate::Confidence::Low => "  [low]",
            };
            out.push_str(&format!(
                "  {}:{} {}{}\n",
                r.display_path, r.line, r.kind, mark
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.references.len(),
                self.total
            ));
        }
        out
    }
}

impl ToolText for crate::navigate::CallHierarchyResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        let render = |label: &str, edges: &[crate::navigate::CallEdge], out: &mut String| {
            if edges.is_empty() {
                return;
            }
            out.push_str(label);
            out.push('\n');
            for e in edges {
                match &e.text {
                    Some(t) => out.push_str(&format!(
                        "  {}:{} {} {}\n",
                        e.display_path, e.line, e.symbol, t
                    )),
                    None => {
                        out.push_str(&format!("  {}:{} {}\n", e.display_path, e.line, e.symbol))
                    }
                }
            }
        };
        render("callers (incoming):", &self.incoming, &mut out);
        render("callees (outgoing):", &self.outgoing, &mut out);
        if out.is_empty() {
            out.push_str(&format!("(no call hierarchy for {})\n", self.symbol));
        }
        out
    }
}

impl ToolText for crate::codemap::CodeStructureResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            out.push_str(&crate::codemap::render_file_codemap(file));
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

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
#[derive(Debug, Deserialize)]
struct SemanticSearchArgs {
    query: String,
    #[serde(default)]
    mode: crate::SemanticSearchMode,
    #[serde(
        default = "default_semantic_max_results",
        deserialize_with = "lenient_usize"
    )]
    max_results: usize,
    #[serde(default = "default_true")]
    rerank: bool,
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
fn default_semantic_max_results() -> usize {
    20
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
fn default_true() -> bool {
    true
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl SemanticSearchArgs {
    fn into_request(self) -> crate::SemanticSearchRequest {
        crate::SemanticSearchRequest {
            query: self.query,
            mode: self.mode,
            max_results: self.max_results,
            rerank: self.rerank,
        }
    }
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
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    context_before: Option<usize>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    context_after: Option<usize>,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default = "default_output_mode")]
    output_mode: String,
}

fn default_output_mode() -> String {
    "content".to_string()
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
            context_before: self.context_before,
            context_after: self.context_after,
            max_content_files: self.max_content_files,
            max_content_bytes: self.max_content_bytes,
            whole_word: self.whole_word,
            include: self.include,
            exclude: self.exclude,
            extensions: self.extensions,
            output_mode: match self.output_mode.as_str() {
                "files_with_matches" | "files" => crate::OutputMode::FilesWithMatches,
                "count" => crate::OutputMode::Count,
                _ => crate::OutputMode::Content,
            },
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

#[derive(Debug, Deserialize)]
struct AstSearchArgs {
    query: String,
    language: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default = "default_ast_max", deserialize_with = "lenient_usize")]
    max_results: usize,
}

fn default_ast_max() -> usize {
    100
}

#[derive(Debug, Deserialize)]
struct AstEditArgs {
    path: String,
    query: String,
    replacement: String,
}

#[derive(Debug, Deserialize)]
struct GitArgs {
    op: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    staged: bool,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default = "default_git_count")]
    count: usize,
    #[serde(default)]
    lines: Option<String>,
}

fn default_git_count() -> usize {
    20
}

/// Run a read-only git subcommand in `root` and return its stdout (capped).
fn run_git(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let bad = |detail: String| {
        DispatchError::Edit(edit::EditError::Parse {
            mode: "git",
            detail,
        })
    };
    if let Some(path) = &args.path
        && path.split(['/', '\\']).any(|segment| segment == "..")
    {
        return Err(bad("path traversal is not allowed".to_string()));
    }
    let mut git: Vec<String> = Vec::new();
    match args.op.as_str() {
        "status" => git.extend(["status", "--short", "--branch"].map(String::from)),
        "diff" => {
            git.push("diff".to_string());
            if args.staged {
                git.push("--staged".to_string());
            }
            if let Some(path) = &args.path {
                git.push("--".to_string());
                git.push(path.clone());
            }
        }
        "log" => {
            git.extend(["log", "--oneline"].map(String::from));
            git.push("-n".to_string());
            git.push(args.count.to_string());
            if let Some(path) = &args.path {
                git.push("--".to_string());
                git.push(path.clone());
            }
        }
        "blame" => {
            let path = args
                .path
                .as_ref()
                .ok_or_else(|| bad("blame requires a path".to_string()))?;
            git.push("blame".to_string());
            if let Some(lines) = &args.lines {
                git.push("-L".to_string());
                git.push(lines.clone());
            }
            git.push("--".to_string());
            git.push(path.clone());
        }
        "show" => {
            let reference = args
                .reference
                .as_ref()
                .ok_or_else(|| bad("show requires a ref".to_string()))?;
            git.push("show".to_string());
            git.push(reference.clone());
        }
        other => return Err(bad(format!("unknown git op: {other}"))),
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&git)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|err| bad(format!("could not run git: {err}")))?;
    if !output.status.success() {
        return Err(bad(format!(
            "git {} failed: {}",
            args.op,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.chars().count() > 20_000 {
        let capped: String = text.chars().take(20_000).collect();
        Ok(format!("{capped}\n\u{2026} (output truncated)\n"))
    } else {
        Ok(text.into_owned())
    }
}

#[derive(serde::Serialize)]
struct AstFileMatch {
    path: String,
    line: usize,
    text: String,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    captures: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
struct AstSearchResponse {
    matches: Vec<AstFileMatch>,
    files_scanned: usize,
}

impl ToolText for AstSearchResponse {
    fn tool_text(&self) -> String {
        if self.matches.is_empty() {
            return format!("(no matches in {} files)\n", self.files_scanned);
        }
        let mut out = String::new();
        for item in &self.matches {
            out.push_str(&format!("{}:{}  {}\n", item.path, item.line, item.text));
        }
        out
    }
}

/// True if `rel_path` is `scope` itself or lives under directory `scope`.
fn path_in_scope(rel_path: &str, scope: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    scope.is_empty() || rel_path == scope || rel_path.starts_with(&format!("{scope}/"))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    diagnostics: Vec<crate::codemap::SyntaxIssue>,
}

impl EditedFile {
    fn with_content(
        action: &'static str,
        path: String,
        moved_to: Option<String>,
        content: &str,
        old: &str,
    ) -> Self {
        let display = moved_to.clone().unwrap_or_else(|| path.clone());
        let diff = (old != content).then(|| unified_diff(&display, old, content));
        Self {
            action,
            tag: Some(edit::snapshot_tag(content)),
            view: Some(edit::hashline_view(&display, content)),
            diff,
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
            if let Some(diff) = &file.diff {
                out.push('\n');
                out.push_str(diff);
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
                EditedFile::with_content("create", path, None, &content, "")
            }
            FileChange::Update { path, content } => {
                let old = read_old(provider, &path);
                provider.write_text(Path::new(&path), &content)?;
                EditedFile::with_content("update", path, None, &content, &old)
            }
            FileChange::Delete { path } => {
                provider.delete_file(Path::new(&path))?;
                EditedFile {
                    action: "delete",
                    path,
                    moved_to: None,
                    tag: None,
                    view: None,
                    diff: None,
                    diagnostics: Vec::new(),
                }
            }
            FileChange::Rename { from, to, content } => {
                let old = read_old(provider, &from);
                provider.rename_file(Path::new(&from), Path::new(&to))?;
                provider.write_text(Path::new(&to), &content)?;
                EditedFile::with_content("rename", from, Some(to), &content, &old)
            }
        };
        files.push(edited);
    }
    Ok(EditResult { files })
}

/// Current text of `path`, or empty if it does not exist / is unreadable.
fn read_old<P: CatalogProvider + ?Sized>(provider: &P, path: &str) -> String {
    provider
        .read_bytes(Path::new(path))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// A compact unified diff (old -> new) for the edit response.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let text_diff = similar::TextDiff::from_lines(old, new);
    let mut builder = text_diff.unified_diff();
    builder
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"));
    let rendered = builder.to_string();
    if rendered.chars().count() > 6000 {
        let capped: String = rendered.chars().take(6000).collect();
        format!("{capped}\n\u{2026} (diff truncated)\n")
    } else {
        rendered
    }
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

#[derive(Debug, Deserialize)]
struct NavigateArgs {
    symbol: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    include_definitions: bool,
    #[serde(default)]
    confident_only: bool,
    #[serde(
        default = "default_nav_max_results",
        deserialize_with = "lenient_usize"
    )]
    max_results: usize,
}

impl NavigateArgs {
    fn into_request(self) -> crate::navigate::NavigateRequest {
        crate::navigate::NavigateRequest {
            symbol: self.symbol,
            language: self.language,
            include_definitions: self.include_definitions,
            confident_only: self.confident_only,
            max_results: self.max_results.max(1),
        }
    }
}

fn default_nav_max_results() -> usize {
    200
}

#[derive(Debug, Deserialize)]
struct CallHierarchyArgs {
    symbol: String,
    #[serde(default = "default_call_direction")]
    direction: crate::navigate::CallDirection,
    #[serde(default)]
    language: Option<String>,
    #[serde(
        default = "default_nav_max_results",
        deserialize_with = "lenient_usize"
    )]
    max_results: usize,
}

fn default_call_direction() -> crate::navigate::CallDirection {
    crate::navigate::CallDirection::Both
}

impl CallHierarchyArgs {
    fn into_request(self) -> crate::navigate::CallHierarchyRequest {
        crate::navigate::CallHierarchyRequest {
            symbol: self.symbol,
            direction: self.direction,
            language: self.language,
            max_results: self.max_results.max(1),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BuildContextArgs {
    query: String,
    #[serde(deserialize_with = "lenient_usize")]
    token_budget: usize,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    max_files: Option<usize>,
    #[serde(default)]
    seed_paths: Vec<PathBuf>,
}

impl BuildContextArgs {
    fn into_request(self) -> crate::BuildContextRequest {
        crate::BuildContextRequest {
            query: self.query,
            token_budget: self.token_budget,
            max_files: self.max_files,
            seed_paths: self.seed_paths,
        }
    }
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
