use serde_json::{Value, json};

/// Return the MCP tool specifications supported by the engine.
#[must_use]
#[allow(clippy::too_many_lines)] // reason: generated/static MCP tool schema block; splitting would obscure byte-stable schema order.
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
                    "patch": { "type": "string", "description": "Full patch text for apply_patch / hashline modes." },
                    "diff_context_lines": { "type": "integer", "default": 3, "description": "Context lines in edit-result unified diffs. Default preserves existing behavior." },
                    "diff_ignore_whitespace": { "type": "boolean", "default": false, "description": "When true, omit paired add/remove lines that differ only by whitespace from edit-result diffs." },
                    "atomic": { "type": "boolean", "default": false, "description": "When true, require all planned file changes to commit as an atomic batch or fail before mutation if unsupported." }
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
            "description": "Structural code search over files of one language. Use mode=\"query\" for raw tree-sitter S-expressions or mode=\"pattern\" for lightweight ast-grep-style code patterns with $META captures.",
            "inputSchema": {
                "type": "object",
                "required": ["language"],
                "properties": {
                    "workspace": workspace_schema(),
                    "mode": { "type": "string", "enum": ["query", "pattern"], "default": "query" },
                    "query": { "type": "string", "description": "Tree-sitter S-expression query, e.g. (call_expression function: (identifier) @name) @match. Raw query mode must capture @match for precise region selection." },
                    "pattern": { "type": "string", "description": "Lightweight code pattern, e.g. foo($ARG). $META captures are returned as metavariables and may be used by ast_edit replacements as ${META}." },
                    "language": { "type": "string", "enum": ["rust", "python", "javascript", "typescript", "tsx", "go", "java", "c", "cpp", "csharp", "ruby", "php"] },
                    "paths": { "type": "array", "items": {"type": "string"}, "description": "Optional file/dir scope relative to a root. Empty = whole catalog." },
                    "max_results": { "type": "integer", "description": "Cap on returned matches (default 100)." }
                }
            }
        }),
        json!({
            "name": "ast_edit",
            "description": "Structural rewrite of one file. Use mode=\"query\" for raw tree-sitter queries or mode=\"pattern\" for lightweight $META code patterns. No write if nothing matches.",
            "inputSchema": {
                "type": "object",
                "required": ["path", "replacement"],
                "properties": {
                    "workspace": workspace_schema(),
                    "path": { "type": "string" },
                    "mode": { "type": "string", "enum": ["query", "pattern"], "default": "query" },
                    "query": { "type": "string", "description": "Tree-sitter query; MUST capture the region to replace as @match." },
                    "pattern": { "type": "string", "description": "Lightweight code pattern, e.g. foo($ARG). $META captures are substituted in replacement with ${META}." },
                    "replacement": { "type": "string", "description": "Template; ${name} -> capture @name's text, and ${META} -> pattern metavariable text." }
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
                    "view": { "type": "string", "enum": ["raw", "hashline", "summary"], "description": "hashline: return the whole file as a [PATH#TAG] header + 1-based N:LINE rows, for authoring hashline edits. summary: return a whole-file structural source summary with elided bodies and concrete re-read ranges. hashline and summary ignore range fields." },
                    "snap": { "type": "string", "enum": ["none", "block"], "default": "none", "description": "Optional syntactic-boundary snapping for raw range reads. block expands a requested range to the smallest containing parsed block when supported; hashline and summary views ignore snap." }
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

#[must_use]
fn workspace_schema() -> Value {
    json!({
        "type": "string",
        "description": "Optional workspace id to route this tool call. Required when multiple workspaces are registered."
    })
}
