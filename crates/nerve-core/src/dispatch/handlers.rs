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
        "list_files" => handle_list_files(provider, arguments, cancel),
        "workspace_context" => handle_workspace_context(provider, arguments, cancel),
        "build_context" => handle_build_context(provider, arguments, cancel),
        "file_search" => handle_file_search(provider, arguments, cancel),
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        "semantic_search" => handle_semantic_search(provider, arguments, cancel),
        "read_file" => handle_read_file(provider, arguments, cancel),
        "edit" => handle_edit(provider, arguments, cancel),
        "replace_symbol_body" => handle_replace_symbol_body(provider, arguments, cancel),
        "insert_before_symbol" => handle_insert_before_symbol(provider, arguments, cancel),
        "insert_after_symbol" => handle_insert_after_symbol(provider, arguments, cancel),
        "rename_symbol" => handle_rename_symbol(provider, arguments, cancel),
        "write" => handle_write(provider, arguments),
        "delete" => handle_delete(provider, arguments),
        "move" => handle_move(provider, arguments),
        "ast_search" => handle_ast_search(provider, arguments, cancel),
        "ast_edit" => handle_ast_edit(provider, arguments),
        "git" => handle_git(provider, arguments, cancel),
        "get_file_tree" => handle_get_file_tree(provider, arguments, cancel),
        "get_code_structure" => handle_get_code_structure(provider, arguments, cancel),
        "get_repo_map" => handle_get_repo_map(provider, arguments, cancel),
        "symbol_search" => handle_symbol_search(provider, arguments, cancel),
        "read_symbol" => handle_read_symbol(provider, arguments, cancel),
        "analyze_impact" => handle_analyze_impact(provider, arguments, cancel),
        "find_referencing_symbols" => handle_find_referencing_symbols(provider, arguments, cancel),
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

fn handle_list_files<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: crate::list_files::ListFilesRequest = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    cancel.check_cancelled()?;
    let response = crate::list_files::list_files(provider, &snapshot, &args)?;
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
    let (request, mut diagnostics) = args.into_request_and_diagnostics()?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let mut response = search_snapshot_cancellable(provider, &snapshot, &request, cancel)?;
    diagnostics.append(&mut response.diagnostics);
    response.diagnostics = diagnostics;
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
        .ok_or(NerveError::SemanticUnavailable)?;
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
        request.snap = None;
    }
    let response = read_file(provider, &request)?;
    cancel.check_cancelled()?;
    match view.as_str() {
        "hashline" => hashline_read_response(response),
        "summary" => summary_read_response(response),
        _ => raw_read_response(response),
    }
}

fn raw_read_response(response: crate::ReadFileResponse) -> Result<Value, DispatchError> {
    let text = response.content.clone();
    let mut structured = serde_json::to_value(&response)?;
    if let Value::Object(fields) = &mut structured {
        fields.insert("content".to_string(), Value::String(text.clone()));
    }
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    }))
}

fn hashline_read_response(response: crate::ReadFileResponse) -> Result<Value, DispatchError> {
    let view = edit::hashline_view(&response.display_path, &response.content);
    Ok(json!({
        "content": [{ "type": "text", "text": view }],
        "structuredContent": {
            "path": response.display_path,
            "content": view,
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
            "content": view,
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
    let (request, diff_options, atomic) = args.into_request_and_diff_options()?;
    let changes = edit::apply(&request, &ProviderReader { provider })?;
    cancel.check_cancelled()?;
    tool_response_text(&apply_changes(provider, changes, diff_options, atomic)?)
}

fn handle_write<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: WriteArgs = serde_json::from_value(arguments)?;
    tool_response_text(&apply_write(provider, args.path, args.content)?)
}

fn handle_delete<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: DeleteArgs = serde_json::from_value(arguments)?;
    tool_response_text(&apply_delete(provider, args.path)?)
}

fn handle_move<P>(provider: &P, arguments: Value) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: MoveArgs = serde_json::from_value(arguments)?;
    tool_response_text(&apply_move(provider, args.from, args.to)?)
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
    tool_response_text(&apply_content_update_with_old(
        provider,
        "ast_edit",
        args.path,
        rewritten,
        source,
        DiffOptions::default(),
    )?)
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
        .ok_or(DispatchError::Core(NerveError::NoRoots))?;
    let output = run_git_response(&root, &args)?;
    Ok(json!({
        "content": [{ "type": "text", "text": output.text }],
        "structuredContent": output.structured,
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
    let selected_mode = args.mode.as_deref() == Some("selected");
    let options = crate::FileTreeOptions {
        mode: crate::TreeMode::from_arg(args.mode.as_deref()),
        max_depth: args.max_depth,
        path: args.path,
    };
    let selection = provider.selection();
    let response = if selected_mode {
        get_selected_file_tree_with_selection(&snapshot, &options, &selection)
    } else {
        get_file_tree_with_selection(&snapshot, &options, &selection)
    };
    tool_response_text(&response)
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

fn handle_symbol_search<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: SymbolSearchArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::symbol_search_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
    tool_response_text(&response)
}

fn handle_read_symbol<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: ReadSymbolArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::read_symbol_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
    tool_response_text(&response)
}

fn handle_analyze_impact<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: ImpactAnalysisArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::analyze_impact_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
    tool_response_text(&response)
}

fn handle_find_referencing_symbols<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: FindReferencingSymbolsArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let response = crate::navigate::find_referencing_symbols_cancellable(
        provider,
        &snapshot,
        &args.into_request(),
        cancel,
    )?;
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
