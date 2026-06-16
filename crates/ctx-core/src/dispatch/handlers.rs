use super::*;
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn dispatch_provider_tool<P>(
    name: &str,
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    match name {
        "manage_selection" => handle_manage_selection(provider, arguments, cancel),
        "workspace_context" => handle_workspace_context(provider, arguments, cancel),
        "build_context" => handle_build_context(provider, arguments, cancel),
        "file_search" => handle_file_search(provider, arguments, cancel),
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        "semantic_search" => handle_semantic_search(provider, arguments, cancel),
        "read_file" => handle_read_file(provider, arguments, cancel),
        "edit" => handle_edit(provider, arguments, cancel),
        "write" => handle_write(provider, arguments),
        "delete" => handle_delete(provider, arguments),
        "move" => handle_move(provider, arguments),
        "ast_search" => handle_ast_search(provider, arguments, cancel),
        "ast_edit" => handle_ast_edit(provider, arguments),
        "git" => handle_git(provider, arguments, cancel),
        "get_file_tree" => handle_get_file_tree(provider, arguments, cancel),
        "get_code_structure" => handle_get_code_structure(provider, arguments, cancel),
        "get_repo_map" => handle_get_repo_map(provider, arguments, cancel),
        "goto_definition" => handle_goto_definition(provider, arguments, cancel),
        "find_references" => handle_find_references(provider, arguments, cancel),
        "call_hierarchy" => handle_call_hierarchy(provider, arguments, cancel),
        other => Err(DispatchError::UnknownTool(other.to_string())),
    }
}

fn handle_manage_selection<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: crate::ManageSelectionRequest = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    cancel.check_cancelled()?;
    let response = manage_selection(provider, &snapshot, &args)?;
    cancel.check_cancelled()?;
    tool_response(serde_json::to_value(response)?)
}

fn handle_workspace_context<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: crate::WorkspaceContextRequest = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    cancel.check_cancelled()?;
    let response = workspace_context(provider, &snapshot, &args)?;
    cancel.check_cancelled()?;
    tool_response_text(&response)
}

fn handle_build_context<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: BuildContextArgs = serde_json::from_value(arguments)?;
    let request = args.into_request();
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    cancel.check_cancelled()?;
    let response = build_context_cancellable(provider, &snapshot, &request, cancel)?;
    cancel.check_cancelled()?;
    tool_response_text(&response)
}

fn handle_file_search<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: FileSearchArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = search_snapshot_cancellable(provider, &snapshot, &args.into_request(), cancel)?;
    tool_response_text(&response)
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
fn handle_semantic_search<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: SemanticSearchArgs = serde_json::from_value(arguments)?;
    let request = args.into_request();
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let index = provider
        .semantic_index()
        .ok_or(CtxError::SemanticUnavailable)?;
    let response = index.search_background((*provider).clone(), snapshot, &request, cancel)?;
    tool_response_text(&response)
}

fn handle_read_file<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    cancel.check_cancelled()?;
    let args: ReadFileArgs = serde_json::from_value(arguments)?;
    let view = args.view.clone().unwrap_or_default();
    let whole_file_view = matches!(view.as_str(), "hashline" | "summary");
    let mut request = args.into_request();
    if whole_file_view {
        request.start_line = None;
        request.end_line = None;
        request.limit = None;
    }
    let response = read_file(provider, &request)?;
    cancel.check_cancelled()?;
    match view.as_str() {
        "hashline" => hashline_read_response(response),
        "summary" => summary_read_response(response),
        _ => tool_response_text(&response),
    }
}

fn hashline_read_response(response: crate::ReadFileResponse) -> Result<Value, DispatchError> {
    let view = edit::hashline_view(&response.display_path, &response.content);
    Ok(json!({
        "content": [{ "type": "text", "text": view }],
        "structuredContent": {
            "path": response.display_path,
            "hashline_tag": edit::snapshot_tag(&response.content),
            "total_lines": response.total_lines,
        },
    }))
}

fn summary_read_response(response: crate::ReadFileResponse) -> Result<Value, DispatchError> {
    let summary = crate::codemap::summarize_source(&response.display_path, &response.content);
    let view = crate::codemap::render_summary(&response.display_path, &summary);
    Ok(json!({
        "content": [{ "type": "text", "text": view }],
        "structuredContent": {
            "path": response.display_path,
            "view": "summary",
            "total_lines": response.total_lines,
            "language": summary.language,
            "parsed": summary.parsed,
            "elided": summary.elided,
            "segments": summary.segments,
        },
    }))
}

fn handle_edit<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    cancel.check_cancelled()?;
    let args: EditArgs = serde_json::from_value(arguments)?;
    let (request, diff_options) = args.into_request_and_diff_options()?;
    let changes = edit::apply(&request, &ProviderReader { provider })?;
    cancel.check_cancelled()?;
    tool_response_text(&apply_changes(provider, changes, diff_options)?)
}

fn handle_write<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: WriteArgs = serde_json::from_value(arguments)?;
    let old = read_old(provider, &args.path);
    provider.write_text(Path::new(&args.path), &args.content)?;
    tool_response_text(&EditResult {
        files: vec![EditedFile::with_content(
            "write",
            args.path,
            None,
            &args.content,
            &old,
            DiffOptions::default(),
        )],
    })
}

fn handle_delete<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: DeleteArgs = serde_json::from_value(arguments)?;
    provider.delete_file(Path::new(&args.path))?;
    tool_response_text(&EditResult {
        files: vec![EditedFile {
            action: "delete",
            path: args.path,
            moved_to: None,
            tag: None,
            view: None,
            diff: None,
            diagnostics: Vec::new(),
        }],
    })
}

fn handle_move<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: MoveArgs = serde_json::from_value(arguments)?;
    provider.rename_file(Path::new(&args.from), Path::new(&args.to))?;
    tool_response_text(&EditResult {
        files: vec![EditedFile {
            action: "move",
            path: args.from,
            moved_to: Some(args.to),
            tag: None,
            view: None,
            diff: None,
            diagnostics: Vec::new(),
        }],
    })
}

fn handle_ast_search<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: AstSearchArgs = serde_json::from_value(arguments)?;
    ensure_ast_language(&args.language, "ast_search")?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = ast_search_response(provider, &snapshot, &args, cancel)?;
    tool_response_text(&response)
}

fn ensure_ast_language(language: &str, mode: &'static str) -> Result<(), DispatchError> {
    if crate::codemap::ast_language_supported(language) {
        return Ok(());
    }
    Err(DispatchError::Edit(edit::EditError::Parse {
        mode,
        detail: format!("unsupported language: {language}"),
    }))
}

fn ast_search_response<P>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    args: &AstSearchArgs,
    cancel: &CancelToken,
) -> Result<AstSearchResponse, DispatchError>
where
    P: DispatchProvider,
{
    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    for entry in &snapshot.entries {
        if !ast_entry_matches_scope(entry, args) {
            continue;
        }
        let Ok(bytes) = provider.read_bytes(&entry.abs_path) else {
            continue;
        };
        files_scanned += 1;
        push_ast_matches(&mut matches, entry, &String::from_utf8_lossy(&bytes), args)?;
        if matches.len() >= args.max_results {
            break;
        }
        cancel.check_cancelled()?;
    }
    Ok(AstSearchResponse {
        matches,
        files_scanned,
    })
}

fn ast_entry_matches_scope(entry: &crate::CatalogEntry, args: &AstSearchArgs) -> bool {
    if crate::codemap::path_language_name(&entry.rel_path) != Some(args.language.as_str()) {
        return false;
    }
    args.paths.is_empty()
        || args
            .paths
            .iter()
            .any(|scope| path_in_scope(&entry.rel_path, scope))
}

enum AstInput<'a> {
    Query(&'a str),
    Pattern(&'a str),
}

fn ast_input<'a>(
    query: &'a Option<String>,
    pattern: &'a Option<String>,
    mode: Option<&str>,
    tool: &'static str,
) -> Result<AstInput<'a>, DispatchError> {
    match (mode, query.as_deref(), pattern.as_deref()) {
        (Some("query"), Some(query), _) | (None, Some(query), _) => Ok(AstInput::Query(query)),
        (Some("pattern"), _, Some(pattern)) | (None, None, Some(pattern)) => {
            Ok(AstInput::Pattern(pattern))
        }
        (Some("query"), None, _) => ast_input_error(tool, "mode `query` requires `query`"),
        (Some("pattern"), _, None) => ast_input_error(tool, "mode `pattern` requires `pattern`"),
        (Some(other), _, _) => ast_input_error(tool, &format!("unknown AST mode: {other}")),
        (None, None, None) => ast_input_error(tool, "provide either `query` or `pattern`"),
    }
}

fn ast_input_error<T>(tool: &'static str, detail: &str) -> Result<T, DispatchError> {
    Err(DispatchError::Edit(edit::EditError::Parse {
        mode: tool,
        detail: detail.to_string(),
    }))
}

fn push_ast_matches(
    matches: &mut Vec<AstFileMatch>,
    entry: &crate::CatalogEntry,
    source: &str,
    args: &AstSearchArgs,
) -> Result<(), DispatchError> {
    let input = ast_input(
        &args.query,
        &args.pattern,
        args.mode.as_deref(),
        "ast_search",
    )?;
    let found = match input {
        AstInput::Query(query) => {
            crate::codemap::ast_search(&entry.rel_path, source, query, args.max_results)
        }
        AstInput::Pattern(pattern) => {
            crate::codemap::ast_search_pattern(&entry.rel_path, source, pattern, args.max_results)
        }
    }
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
    Ok(())
}

fn handle_ast_edit<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: AstEditArgs = serde_json::from_value(arguments)?;
    let bytes = provider.read_bytes(Path::new(&args.path))?;
    let source = String::from_utf8_lossy(&bytes).into_owned();
    let (rewritten, count) = ast_rewrite(&args, &source)?;
    if count == 0 {
        return Ok(json!({
            "content": [{ "type": "text", "text": format!("ast_edit: no matches in {}\n", args.path) }],
            "structuredContent": { "path": args.path, "rewrites": 0 },
        }));
    }
    provider.write_text(Path::new(&args.path), &rewritten)?;
    tool_response_text(&EditResult {
        files: vec![EditedFile::with_content(
            "ast_edit",
            args.path,
            None,
            &rewritten,
            &source,
            DiffOptions::default(),
        )],
    })
}

fn ast_rewrite(args: &AstEditArgs, source: &str) -> Result<(String, usize), DispatchError> {
    let input = ast_input(&args.query, &args.pattern, args.mode.as_deref(), "ast_edit")?;
    let result = match input {
        AstInput::Query(query) => {
            crate::codemap::ast_rewrite(&args.path, source, query, &args.replacement)
        }
        AstInput::Pattern(pattern) => {
            crate::codemap::ast_rewrite_pattern(&args.path, source, pattern, &args.replacement)
        }
    };
    result.map_err(|detail| {
        DispatchError::Edit(edit::EditError::Parse {
            mode: "ast_edit",
            detail,
        })
    })
}

fn handle_git<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: GitArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let root = snapshot
        .roots
        .first()
        .map(|root| root.path.clone())
        .ok_or(DispatchError::Core(CtxError::NoRoots))?;
    let output = run_git(&root, &args)?;
    Ok(json!({
        "content": [{ "type": "text", "text": output.clone() }],
        "structuredContent": { "op": args.op, "output": output },
    }))
}

fn handle_get_file_tree<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: FileTreeArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    cancel.check_cancelled()?;
    let options = crate::FileTreeOptions {
        mode: crate::TreeMode::from_arg(args.mode.as_deref()),
        max_depth: args.max_depth,
        path: args.path,
    };
    tool_response_text(&get_file_tree(&snapshot, &options))
}

fn handle_get_code_structure<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: CodeStructureArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    cancel.check_cancelled()?;
    let response = get_code_structure(provider, &snapshot, &args.paths.unwrap_or_default())?;
    cancel.check_cancelled()?;
    tool_response_text(&response)
}

fn handle_get_repo_map<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: RepoMapArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = get_repo_map_cancellable(provider, &snapshot, &args.into_request(), cancel)?;
    tool_response_text(&response)
}

fn handle_goto_definition<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: NavigateArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::goto_definition_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
    tool_response_text(&response)
}

fn handle_find_references<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: NavigateArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::find_references_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
    tool_response_text(&response)
}

fn handle_call_hierarchy<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: CallHierarchyArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::call_hierarchy_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
    tool_response_text(&response)
}
