use crate::RuntimeError;
use nerve_core::dispatch::DispatchProvider;
use nerve_core::{CancelToken, WorkspaceResolver};
use serde_json::Value;

// The advisory risk/capability vocabulary is transport-neutral protocol data and
// lives in `nerve-proto`; the trait below is engine-coupled (it references the
// nerve-core ports) so it stays here.
pub use nerve_proto::{RiskTier, ToolCapability};

/// Extension point for host-specific or provider-specific runtime capabilities.
///
/// Adapters are consulted in registration order. The first adapter returning
/// `Ok(Some(_))` claims the tool call. Returning `Ok(None)` means the adapter
/// does not own the requested tool, so runtime dispatch continues.
pub trait RuntimeToolAdapter<R>: Send + Sync
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    /// Tool specifications exposed by this adapter.
    fn tool_specs(&self) -> Vec<Value>;

    /// Try to handle one MCP-style `tools/call` params object.
    fn handle_tool_call(&self, resolver: &R, params: &Value)
    -> Result<Option<Value>, RuntimeError>;

    /// Try to handle one MCP-style `tools/call` params object with cooperative cancellation.
    fn handle_tool_call_cancellable(
        &self,
        resolver: &R,
        params: &Value,
        cancel: &CancelToken,
    ) -> Result<Option<Value>, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::cancelled());
        }
        self.handle_tool_call(resolver, params)
    }

    /// Declared capability/risk descriptor for the named tool. Advisory only;
    /// defaults to the most permissive [`ToolCapability`] so existing adapters
    /// are unaffected. A future permission engine (P4) consults this.
    fn tool_capability(&self, _name: &str) -> ToolCapability {
        ToolCapability::default()
    }

    /// Whether this adapter owns (claims) the named tool. Defaults to `false`
    /// so the descriptor query is opt-in and never changes dispatch ordering,
    /// which still relies on `handle_tool_call` returning `Ok(Some(_))`.
    fn owns(&self, _name: &str) -> bool {
        false
    }
}
