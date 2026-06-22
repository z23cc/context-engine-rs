//! Transport-neutral MCP tool dispatch for the context engine.

mod args;
mod ast;
mod batch;
mod editing;
mod error;
mod git;
mod handlers;
mod impact_args;
mod read_symbol_args;
mod referencing_symbols_args;
mod specs;
mod symbol_edit;
mod symbol_rename;
mod symbol_rename_scope;
mod text;
mod tool_search;

use args::*;
use ast::*;
use batch::*;
use editing::*;
pub use error::{
    DispatchError, dispatch_error_json, dispatch_error_json_for, dispatch_error_kind,
    dispatch_error_value,
};
use git::run_git_response;
use handlers::dispatch_provider_tool;
use impact_args::*;
use read_symbol_args::*;
use referencing_symbols_args::*;
pub use specs::tool_specs;
use symbol_edit::*;
use symbol_rename::*;
use symbol_rename_scope::*;
#[cfg(test)]
use text::{REPO_MAP_TEXT_BUDGET_CHARS, render_repo_map_text};
use text::{ToolText, tool_response, tool_response_text};
use tool_search::search_tool_specs;

use crate::edit;
use crate::{
    CancelToken, CatalogProvider, NerveError, SingletonWorkspaceResolver, WorkspaceResolver,
    build_context_cancellable, get_code_structure, get_file_tree_with_selection,
    get_repo_map_cancellable, get_selected_file_tree_with_selection, manage_selection, read_file,
    search_snapshot_cancellable, workspace_context,
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
    if name == "tool_search" {
        let args: ToolSearchArgs = serde_json::from_value(arguments)?;
        return tool_response_text(&search_tool_specs(args));
    }
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
        Err(err) if matches!(err, DispatchError::Core(NerveError::Cancelled)) => Ok(
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
mod tests;
