//! Shared composition for the daemon transports.
//!
//! Both the stdio and HTTP transports build the *same* transport-neutral
//! [`RuntimeDaemonRouter`] (runtime + provider registry + policy + session
//! store) from `ServeArgs`, parameterised only by where runtime notifications
//! are emitted. Keeping this in one place is what guarantees every transport
//! speaks the identical Protocol v3 (architecture north star §3: single
//! protocol authority; the transport is the only thing that varies).

use super::router::RuntimeDaemonRouter;
use crate::session::SessionStore;
use crate::{policy, providers::ProviderRegistry, tools, workspace};
use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

/// Build the runtime daemon router for a transport, wiring its notification
/// sink to `emit_notification` (stdout for stdio, the SSE hub for HTTP).
///
/// `allow_delegate` lifts the daemon's default refusal of `delegate.start` jobs:
/// when set, the router injects a real (process) exec launcher for the delegate
/// path *only* (the agent-run / session exec posture stays refusing). Off by
/// default — the daemon refuses to spawn external agents.
pub(super) fn build_router(
    serve_args: &workspace::ServeArgs,
    allow_delegate: bool,
    emit_notification: impl Fn(Value) + Send + Sync + 'static,
) -> Result<RuntimeDaemonRouter> {
    let registry = ProviderRegistry::from_args(serve_args)?;
    let policy = policy::daemon_policy(serve_args)?;
    let runtime = tools::runtime(workspace::registry(serve_args)?);
    let runtime = Arc::new(crate::mcp::attach(runtime, serve_args)?);
    // P5: persist daemon `agent.run` transcripts under the served project's
    // `.nerve/sessions` (global config home when no root is served).
    let session_store =
        SessionStore::for_scope(serve_args.roots.first().map(|root| root.as_path())).ok();
    // Trust context for delegation: a real launcher only under --allow-delegate.
    let delegate_launcher = if allow_delegate {
        crate::sandbox::process_launcher()
    } else {
        crate::sandbox::refuse_launcher()
    };
    Ok(RuntimeDaemonRouter::new(
        runtime,
        registry,
        policy,
        session_store,
        delegate_launcher,
        emit_notification,
    ))
}
