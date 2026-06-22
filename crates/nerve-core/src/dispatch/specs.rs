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
            "name": "replace_symbol_body",
            "description": "Replace the full enclosing definition block for one exact symbol, using deterministic tree-sitter codemaps. Use after read_symbol include_body=true so the replacement body is a whole function/class/method definition. If the symbol is ambiguous, no mutation occurs; refine with path, language, or kind.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol", "body"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "path": { "type": "string", "description": "Optional file or directory scope relative to an allowed root, or root-id-prefixed display path in multi-root workspaces." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive symbol kind filter, e.g. function, struct, class, method." },
                    "body": { "type": "string", "description": "Replacement full symbol definition. Leading/trailing blank lines are stripped; indentation is preserved." }
                }
            }
        }),
        json!({
            "name": "insert_before_symbol",
            "description": "Insert content before the full enclosing definition block for one exact symbol, using deterministic tree-sitter codemaps. If the symbol is ambiguous, no mutation occurs; refine with path, language, or kind.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol", "body"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "path": { "type": "string", "description": "Optional file or directory scope relative to an allowed root, or root-id-prefixed display path in multi-root workspaces." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive symbol kind filter, e.g. function, struct, class, method." },
                    "body": { "type": "string", "description": "Content to insert before the symbol definition. Line endings are normalized to the target file; a trailing newline is added when needed." }
                }
            }
        }),
        json!({
            "name": "insert_after_symbol",
            "description": "Insert content after the full enclosing definition block for one exact symbol, using deterministic tree-sitter codemaps. If the symbol is ambiguous, no mutation occurs; refine with path, language, or kind.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol", "body"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "path": { "type": "string", "description": "Optional file or directory scope relative to an allowed root, or root-id-prefixed display path in multi-root workspaces." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive symbol kind filter, e.g. function, struct, class, method." },
                    "body": { "type": "string", "description": "Content to insert after the symbol definition. Line endings are normalized to the target file; a trailing newline is added when needed." }
                }
            }
        }),
        json!({
            "name": "rename_symbol",
            "description": "Conservatively rename one exact symbol definition, same-file references, and single-root import-backed cross-file references using codemap line:column occurrences. Cross-file name-only hits without a resolvable import are intentionally not mutated. For aliased imports, only the imported source specifier is renamed; local aliases and alias call sites are preserved. If the target definition is ambiguous, references are truncated, or the current source no longer matches the recorded occurrences, no mutation occurs.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol", "new_name"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact old symbol name (case-sensitive)." },
                    "new_name": { "type": "string", "description": "New ASCII identifier name. Conservative validation rejects whitespace, punctuation, empty names, and common language keywords." },
                    "path": { "type": "string", "description": "Optional file or directory scope for selecting the unique old definition." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive symbol kind filter, e.g. function, class, method." }
                }
            }
        }),
        json!({
            "name": "write",
            "description": "Create or overwrite a file with exact content (within allowed roots).",
            "inputSchema": { "type": "object", "required": ["path", "content"], "properties": { "workspace": workspace_schema(), "path": {"type": "string"}, "content": {"type": "string"} } }
        }),
        json!({
            "name": "tool_search",
            "description": "Search Nerve's tool catalog by intent, tool name, parameter name, or schema description. Use this when you are unsure which built-in tool to call; returns compact ranked matches with full details in structuredContent.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Intent or capability to find, e.g. syntax diagnostics, git diff, selection slices, repo map." },
                    "max_results": { "type": "integer", "default": 8 }
                }
            }
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
            "description": "Structural code search over files of one language, including supported Markdown fenced-code snippets on host line numbers. Use mode=\"query\" for raw tree-sitter S-expressions or mode=\"pattern\" for lightweight $META pattern matching.",
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
            "description": "Read-only git in the workspace root: op = status | diff | log | blame | show. diff takes optional path + staged + detail (summary/files/patches/bundle/full); detail=patches returns churn-sorted bounded patches; detail=bundle returns compact text plus structured changed files and bounded patch payload; log takes count + path; blame takes path (+ lines L-range); show takes ref." ,
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "workspace": workspace_schema(),
                    "op": { "type": "string", "enum": ["status", "diff", "log", "blame", "show"] },
                    "path": { "type": "string" },
                    "staged": { "type": "boolean", "description": "diff: staged vs HEAD." },
                    "detail": { "type": "string", "enum": ["summary", "files", "patches", "bundle", "full"], "description": "diff: output detail. summary uses --shortstat; files lists churn-sorted files; patches emits churn-sorted bounded patches in text; bundle emits compact text plus structured changed files and bounded patch payload; full is the default raw diff." },
                    "max_chars": { "type": "integer", "description": "Maximum git output characters before a truncation note (default 20000; must be greater than 0)." },
                    "ref": { "type": "string", "description": "show: commit/ref." },
                    "count": { "type": "integer", "description": "log: number of commits (default 20)." },
                    "lines": { "type": "string", "description": "blame: line range, e.g. 10,20." }
                }
            }
        }),
        json!({
            "name": "build_context",
            "description": "Build a deterministic query-focused context within a token budget. Manifest includes ranking/selection details, per-signal score breakdowns, allocation/budget trace entries, and sensitive-content diagnostics for included context.",
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
            "name": "list_files",
            "description": "List the workspace's files as structured rows (root-relative path, display path, selected) for a clickable file picker/tree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": workspace_schema(),
                    "query": { "type": "string", "description": "Case-insensitive substring filter over the root-relative path." },
                    "limit": { "type": "integer", "description": "Maximum rows to return (default 4000)." }
                }
            }
        }),
        json!({
            "name": "workspace_context",
            "description": "Assemble the current persistent selection into context text with token breakdowns and deterministic context/file content hashes for downstream cache keys. Optional sections include selected tree and code structure. A named recipe (standard|plan|review|diff|manual) fixes the section set.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": workspace_schema(),
                    "recipe": {
                        "type": "string",
                        "enum": ["standard", "plan", "review", "diff", "manual"],
                        "description": "Named context recipe; fixes the section set (overrides `include`)."
                    },
                    "include": {
                        "type": "array",
                        "items": { "type": "string", "enum": ["file-map", "tree", "code", "contents", "tokens", "git-diff", "meta-prompts"] },
                        "description": "Sections to include when no recipe is set. Empty means file-map and contents. tree renders the selected-only file tree; code renders selected codemaps."
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Optional notes/instructions appended as the last section."
                    },
                    "git_diff": {
                        "type": "string",
                        "description": "Working-tree diff text for the git_diff section. The caller supplies it (e.g. from the git tool); the kernel never runs git."
                    },
                    "meta_prompts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "required": ["title", "body"],
                            "properties": { "title": { "type": "string" }, "body": { "type": "string" } }
                        },
                        "description": "Reusable instruction blocks rendered as numbered meta-prompt sections (else a recipe's defaults)."
                    }
                }
            }
        }),
        json!({
            "name": "manage_selection",
            "description": "Persist and summarize the selected file set with token estimates. Supports dry-run preview, opt-in auto codemap expansion, and full/slices/codemap_only mode conversion via promote/demote.",
            "inputSchema": {
                "type": "object",
                "required": ["op"],
                "properties": {
                    "workspace": workspace_schema(),
                    "op": { "type": "string", "enum": ["get", "add", "remove", "set", "clear", "preview", "promote", "demote"], "description": "preview is a non-mutating current/add summary; promote converts selected targets to full; demote converts selected targets to codemap_only. promote/demote without paths apply to all selected files." },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File or directory paths relative to an allowed root, root-prefixed as root-name/path or root-id/path, or in-root absolute paths."
                    },
                    "mode": { "type": "string", "enum": ["full", "slices", "codemap_only"], "default": "full" },
                    "auto_codemap": { "type": "boolean", "default": false, "description": "When true on add/set/preview with full or slices targets, auto-add up to 8 same-root codemap_only files defining symbols referenced by the current request's targets. Default false preserves manual token budgets." },
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
                                            "end_line": { "type": "integer" },
                                            "label": { "type": "string", "description": "Optional purpose label rendered in workspace_context slice descriptions. Provide only one of label/description/desc." },
                                            "description": { "type": "string", "description": "Alias for label; provide only one label alias." },
                                            "desc": { "type": "string", "description": "Alias for label; provide only one label alias." }
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
                    "regex_fallback": { "type": "string", "enum": ["error", "literal"], "default": "error", "description": "When regex=true and the pattern is invalid: error keeps strict behavior; literal falls back to a literal search and emits a diagnostic." },
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
                    "snap": { "type": "string", "enum": ["none", "block"], "default": "none", "description": "Optional syntactic-boundary snapping for raw range reads. block expands a requested range to the smallest containing parsed block when supported, including supported fenced code in Markdown; hashline and summary views ignore snap." }
                }
            }
        }),
        json!({
            "name": "get_file_tree",
            "description": "Return a compact ASCII directory tree for allowed roots. `auto` mode (default) adapts depth/breadth to a size budget; `selected` shows only current selection plus parent directories and ignores `max_depth` so selected files remain visible; pass `path` to scope to a subdirectory on large repos. File markers: `*` currently selected, `+` codemap-capable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "workspace": workspace_schema(),
                    "mode": { "type": "string", "enum": ["auto", "full", "folders", "selected"], "default": "auto", "description": "auto: fit a size budget (degrades depth->folders->top-level, with a note). full: everything (can be large). folders: directories only. selected: selected files plus parent directories, ignoring max_depth." },
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
            "name": "symbol_search",
            "description": "Fuzzy/partial search for symbols across the workspace using deterministic tree-sitter codemaps. Use before goto_definition when the exact symbol name is unknown; filters by display language and symbol kind. Not a scope/type resolver.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "workspace": workspace_schema(),
                    "query": { "type": "string", "description": "Partial symbol/name intent, e.g. payment gateway, process pay, auth middleware." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive symbol kind filter, e.g. function, struct, class, method." },
                    "max_results": { "type": "integer", "default": 50, "description": "Maximum matches returned; 0 returns totals only." }
                }
            }
        }),
        json!({
            "name": "read_symbol",
            "description": "Read the enclosing source block for one exact symbol definition using deterministic tree-sitter codemaps. If the name is ambiguous, returns compact candidate locations instead of a body; refine with path, language, or kind. Not a scope/type resolver.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact symbol name (case-sensitive)." },
                    "path": { "type": "string", "description": "Optional file or directory scope relative to an allowed root, or root-id-prefixed display path in multi-root workspaces." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive symbol kind filter, e.g. function, struct, class, method." },
                    "include_body": { "type": "boolean", "default": true, "description": "When true, include the enclosing body only if exactly one symbol match remains." },
                    "max_matches": { "type": "integer", "default": 20, "description": "Maximum ambiguous candidates returned." }
                }
            }
        }),
        json!({
            "name": "analyze_impact",
            "description": "Analyze a bounded reverse dependency graph for an exact symbol: enclosing symbols that reference it, then symbols that reference those symbols up to max_depth. Deterministic tree-sitter name matching; not a scope/type resolver, so ambiguous names are surfaced with confidence.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact seed symbol name (case-sensitive)." },
                    "path": { "type": "string", "description": "Optional seed definition file or directory scope relative to an allowed root, or root-id-prefixed display path in multi-root workspaces." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive seed symbol kind filter, e.g. function, struct, class, method." },
                    "max_depth": { "type": "integer", "default": 2, "description": "Reverse-dependency depth. 1 returns direct dependents only." },
                    "max_results": { "type": "integer", "default": 200, "description": "Maximum impacted symbols returned." },
                    "confident_only": { "type": "boolean", "default": false, "description": "Drop low-confidence name-only matches when true." }
                }
            }
        }),
        json!({
            "name": "find_referencing_symbols",
            "description": "Find enclosing symbols that reference an exact target symbol, with the exact reference line and a small source context. This is a compact symbolic-read view between raw find_references and full analyze_impact. Deterministic tree-sitter name matching; not a scope/type resolver.",
            "inputSchema": {
                "type": "object",
                "required": ["symbol"],
                "properties": {
                    "workspace": workspace_schema(),
                    "symbol": { "type": "string", "description": "Exact target symbol name (case-sensitive)." },
                    "path": { "type": "string", "description": "Optional target definition file or directory scope relative to an allowed root, or root-id-prefixed display path in multi-root workspaces." },
                    "language": { "type": "string", "description": "Optional display-language filter, e.g. rust, typescript, tsx, python." },
                    "kind": { "type": "string", "description": "Optional case-insensitive target symbol kind filter, e.g. function, struct, class, method." },
                    "confident_only": { "type": "boolean", "default": false, "description": "Drop low-confidence name-only referencing symbols when true." },
                    "context_lines": { "type": "integer", "default": 1, "description": "Lines before/after the exact reference line to include, capped at 5." },
                    "max_results": { "type": "integer", "default": 200, "description": "Maximum referencing-symbol entries returned." }
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
