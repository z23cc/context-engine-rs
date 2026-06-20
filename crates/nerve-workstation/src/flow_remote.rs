//! Production transport for the C6 remote/MCP worker adapters (design §10 C6).
//!
//! The [`RemoteWorker`](crate::worker::RemoteWorker) /
//! [`McpWorker`](crate::worker::McpWorker) adapters are wired and hermetically tested
//! against in-process fakes (see `crate::worker::remote`). The PRODUCTION transports
//! are the documented follow-on:
//!
//! - **Remote:** spawn `nerve daemon --stdio` and drive it over the runtime protocol
//!   using the existing protocol client (the `nerve-tui` `NerveClient` is the
//!   reference): `flow.start` a (sub)flow on the remote daemon, subscribe to its
//!   `flow_*` events, and re-project each onto a [`WorkerEvent`](crate::worker::WorkerEvent).
//!   This reuses the shipped client + protocol verbatim — no new transport.
//! - **MCP:** connect an MCP client to the named server (the P1 MCP-client
//!   `RuntimeToolAdapter` seam) and call its tool.
//!
//! Until those land, [`FollowOnConnector`] returns a clear, honest error so an
//! operator who opens the fleet (`--allow-delegate`) and references a `remote`/`mcp`
//! worker sees exactly why it is not yet runnable — rather than a silent stub. The
//! adapter SHAPE + the security gate (refused-by-default at the factory) are real and
//! tested; this is the one documented gap.

use crate::worker::{McpEndpoint, RemoteConnector, RemoteEndpoint, WorkerError};
use std::sync::Arc;

/// The production connector: the gate is real (it is only constructed when the fleet
/// was explicitly opened), the adapter shape is real, but the wire to a real remote
/// daemon / MCP server is the documented follow-on. Resolving a `remote`/`mcp` worker
/// therefore surfaces a clear "transport not yet wired" error instead of a stub.
pub(crate) struct FollowOnConnector;

impl RemoteConnector for FollowOnConnector {
    fn remote(&self, endpoint: &str) -> Result<Arc<dyn RemoteEndpoint>, WorkerError> {
        Err(WorkerError::Start(format!(
            "remote worker `{endpoint}` is enabled but its production transport is a \
             follow-on: spawn `nerve daemon --stdio` and drive it over the runtime \
             protocol (the nerve-tui NerveClient is the reference). The adapter + the \
             security gate are shipped + tested; only the wire is pending."
        )))
    }

    fn mcp(&self, server: &str) -> Result<Arc<dyn McpEndpoint>, WorkerError> {
        Err(WorkerError::Start(format!(
            "mcp worker `{server}` is enabled but its production transport is a \
             follow-on: connect an MCP client to the server (the P1 MCP-client \
             RuntimeToolAdapter seam). The adapter + the security gate are shipped + \
             tested; only the wire is pending."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::CancelToken;

    #[test]
    fn follow_on_connector_reports_the_pending_transport_clearly() {
        let connector = FollowOnConnector;
        match connector.remote("peer") {
            Ok(_) => panic!("remote transport is a follow-on"),
            Err(err) => assert!(err.to_string().contains("follow-on"), "{err}"),
        }
        match connector.mcp("srv") {
            Ok(_) => panic!("mcp transport is a follow-on"),
            Err(err) => assert!(err.to_string().contains("follow-on"), "{err}"),
        }
    }

    // The CancelToken import keeps the hermetic-fake reference in `worker::remote`
    // discoverable from this module's docs; exercise it so it is not dead.
    #[test]
    fn cancel_token_constructs() {
        let _ = CancelToken::never();
    }
}
