//! Remote + MCP-client worker adapters (Wave C6, design §10) — proving the
//! [`AgentWorker`] port's generality with ZERO engine change.
//!
//! C0 shipped two [`AgentWorker`] families (CLI subprocess, in-process provider).
//! C6 adds two more, both reaching an EXTERNAL agentic endpoint, to prove the port
//! extends without the engine learning a new shape:
//!
//! - [`RemoteWorker`] drives ANOTHER `nerve daemon` over the runtime protocol
//!   (Nerve already has a protocol client — see `nerve-tui`): start a session/flow
//!   on the remote daemon and stream its events back as [`WorkerEvent`]. The
//!   production transport (spawn `nerve daemon --stdio`, JSON-RPC over NDJSON) is a
//!   documented follow-on; C6 ships the adapter SHAPE behind a [`RemoteEndpoint`]
//!   seam + a hermetic in-process fake.
//! - [`McpWorker`] consumes an MCP server as a worker (an MCP-client-backed worker):
//!   call a tool on a connected MCP server and project its result as a turn. Same
//!   shape + a hermetic [`McpEndpoint`] fake; the production MCP-client transport is
//!   the documented follow-on.
//!
//! ## SECURITY BEFORE OPENNESS (design §6/§9 — REQUIRED)
//!
//! Both are EXEC-TIER ([`RiskTier::Exec`]): a remote daemon or an MCP server can run
//! arbitrary actions Nerve cannot see. So the [`WorkerFactory`](super::factory)
//! REFUSES to mint either unless the fleet was explicitly opened (the same
//! `--allow-delegate` posture a CLI worker passes). The refusal is enforced at the
//! factory (mint) boundary, NOT here — these types are pure adapters; the gate is the
//! outermost boundary (north-star invariant 9). [`super::factory`] tests the
//! refused-by-default contract.

use super::{
    AgentWorker, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerKind, WorkerSession,
    WorkerTask, synthesize_turn_steps,
};
use nerve_core::CancelToken;
use nerve_runtime::{AgentEventKind, RiskTier};
use std::sync::Arc;

/// The transport a [`RemoteWorker`] drives. A turn sends a prompt to a remote
/// `nerve daemon` (or other protocol peer) and returns its streamed events + final
/// result. Abstracted so the production transport (spawn `nerve daemon --stdio`,
/// JSON-RPC/NDJSON via the existing protocol client) and a hermetic in-process fake
/// share one adapter — the port's generality is proven by the SHAPE, not the wire.
pub(crate) trait RemoteEndpoint: Send + Sync {
    /// Run one turn against the remote peer: send `prompt`, stream each remote step
    /// into `on_event`, and return the final structured result. `cancel` lets the
    /// engine abort a slow remote turn cooperatively.
    fn turn(
        &self,
        prompt: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError>;
}

/// A worker that drives a remote `nerve daemon` (design §10 C6). Exec-tier and
/// refused-by-default at mint time (the factory gates it; this adapter is pure). The
/// endpoint is injected so the production protocol client and a hermetic fake are
/// interchangeable.
pub(crate) struct RemoteWorker {
    endpoint: Arc<dyn RemoteEndpoint>,
    /// A stable label for the remote peer (e.g. the endpoint string), surfaced in
    /// [`WorkerKind`] so the engine/ledger stay kind-agnostic.
    label: String,
}

impl RemoteWorker {
    /// Build a remote worker over `endpoint`, labelled by the remote peer's address.
    pub(crate) fn new(endpoint: Arc<dyn RemoteEndpoint>, label: impl Into<String>) -> Self {
        Self {
            endpoint,
            label: label.into(),
        }
    }
}

impl AgentWorker for RemoteWorker {
    fn kind(&self) -> WorkerKind {
        // A remote worker is, from the engine's view, a CLI-shaped opaque peer: it
        // produces a turn we cannot introspect, exactly like a delegated CLI. Reusing
        // the `Cli` discriminant keeps the engine kind-agnostic (no new variant).
        WorkerKind::Cli("remote")
    }

    fn capability(&self) -> RiskTier {
        // A remote daemon can reach anything its own policy allows — worst-case exec.
        RiskTier::Exec
    }

    fn start(
        &self,
        task: &WorkerTask,
        _ctx: &WorkerContext,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let result = self.endpoint.turn(&task.prompt, cancel, on_event)?;
        Ok(Box::new(RemoteSession {
            endpoint: Arc::clone(&self.endpoint),
            label: self.label.clone(),
            last: result,
        }))
    }
}

/// A live remote session: a steer runs another turn against the same endpoint, so a
/// remote daemon that retains its own session continues it. The label is carried so
/// the session is self-describing in logs.
struct RemoteSession {
    endpoint: Arc<dyn RemoteEndpoint>,
    #[allow(
        dead_code,
        reason = "carried for self-describing logs / future routing"
    )]
    label: String,
    last: TurnResult,
}

impl WorkerSession for RemoteSession {
    fn steer(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        let result = self.endpoint.turn(message, cancel, on_event)?;
        self.last = result.clone();
        Ok(result)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

/// The transport an [`McpWorker`] consumes: call a tool on a connected MCP server
/// and return its textual result. Abstracted so the production MCP client (the P1
/// MCP-client `RuntimeToolAdapter` seam) and a hermetic fake share one adapter.
pub(crate) trait McpEndpoint: Send + Sync {
    /// Invoke the worker's MCP tool with `prompt` as input; return the tool's text
    /// result. `cancel` aborts a slow call cooperatively.
    fn call(&self, prompt: &str, cancel: &CancelToken) -> Result<String, WorkerError>;
}

/// A worker backed by an MCP-server tool (design §10 C6). Exec-tier and
/// refused-by-default at mint time. One MCP `call` IS a turn: the tool's text result
/// becomes the turn's message, synthesized into the canonical step stream so the
/// engine sees the same shape as any other worker.
pub(crate) struct McpWorker {
    endpoint: Arc<dyn McpEndpoint>,
    server: String,
}

impl McpWorker {
    /// Build an MCP worker over `endpoint`, labelled by the MCP `server` it consumes.
    pub(crate) fn new(endpoint: Arc<dyn McpEndpoint>, server: impl Into<String>) -> Self {
        Self {
            endpoint,
            server: server.into(),
        }
    }

    /// Run one MCP call and synthesize its canonical step stream + result.
    fn run_turn(
        &self,
        turn: u64,
        prompt: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        let text = self.endpoint.call(prompt, cancel)?;
        let result = TurnResult {
            ok: true,
            text,
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        };
        synthesize_turn_steps(turn, &result, on_event);
        Ok(result)
    }
}

impl AgentWorker for McpWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Cli("mcp")
    }

    fn capability(&self) -> RiskTier {
        // An MCP server can perform arbitrary side effects — worst-case exec.
        RiskTier::Exec
    }

    fn start(
        &self,
        task: &WorkerTask,
        _ctx: &WorkerContext,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let result = self.run_turn(1, &task.prompt, cancel, on_event)?;
        Ok(Box::new(McpSession {
            endpoint: Arc::clone(&self.endpoint),
            server: self.server.clone(),
            last: result,
        }))
    }
}

/// A live MCP session: a steer is another tool call on the same server.
struct McpSession {
    endpoint: Arc<dyn McpEndpoint>,
    #[allow(
        dead_code,
        reason = "carried for self-describing logs / future routing"
    )]
    server: String,
    last: TurnResult,
}

impl WorkerSession for McpSession {
    fn steer(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        let text = self.endpoint.call(message, cancel)?;
        let result = TurnResult {
            ok: true,
            text,
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        };
        synthesize_turn_steps(2, &result, on_event);
        self.last = result.clone();
        Ok(result)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hermetic remote endpoint: echoes the prompt back as a remote turn, emitting
    /// a real `AgentEventKind::Message` step (the same shape a real remote daemon's
    /// `SessionAgent` would carry) before the structured result.
    struct FakeRemote;
    impl RemoteEndpoint for FakeRemote {
        fn turn(
            &self,
            prompt: &str,
            _cancel: &CancelToken,
            on_event: &mut dyn FnMut(WorkerEvent),
        ) -> Result<TurnResult, WorkerError> {
            on_event(WorkerEvent::Step(AgentEventKind::Message {
                text: format!("remote saw: {prompt}"),
            }));
            Ok(TurnResult {
                ok: true,
                text: format!("remote done: {prompt}"),
                usage: nerve_agent::Usage::default(),
                cost_usd: None,
                timed_out: false,
            })
        }
    }

    #[test]
    fn remote_worker_emits_worker_events_through_the_port() {
        let worker = RemoteWorker::new(Arc::new(FakeRemote), "peer-1");
        assert_eq!(worker.kind(), WorkerKind::Cli("remote"));
        assert_eq!(worker.capability(), RiskTier::Exec);
        let task = WorkerTask {
            prompt: "investigate the bug".into(),
            autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
            model: None,
            tool_filter: None,
            budget: super::super::BudgetGrant::default(),
        };
        let ctx = ctx();
        let mut events = Vec::new();
        let session = worker
            .start(&task, &ctx, &CancelToken::never(), &mut |e| events.push(e))
            .expect("remote start");
        // The remote turn streamed a real WorkerEvent::Step (the port's contract).
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Step(AgentEventKind::Message { text }) if text == "remote saw: investigate the bug"
        )));
        assert_eq!(session.result().text, "remote done: investigate the bug");
    }

    /// A hermetic MCP endpoint returning a canned tool result.
    struct FakeMcp;
    impl McpEndpoint for FakeMcp {
        fn call(&self, prompt: &str, _cancel: &CancelToken) -> Result<String, WorkerError> {
            Ok(format!("mcp result for: {prompt}"))
        }
    }

    #[test]
    fn mcp_worker_projects_a_tool_call_as_a_turn() {
        let worker = McpWorker::new(Arc::new(FakeMcp), "some-mcp");
        assert_eq!(worker.kind(), WorkerKind::Cli("mcp"));
        assert_eq!(worker.capability(), RiskTier::Exec);
        let task = WorkerTask {
            prompt: "summarize".into(),
            autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
            model: None,
            tool_filter: None,
            budget: super::super::BudgetGrant::default(),
        };
        let ctx = ctx();
        let mut events = Vec::new();
        let session = worker
            .start(&task, &ctx, &CancelToken::never(), &mut |e| events.push(e))
            .expect("mcp start");
        // The MCP result became the turn's Message step + structured result.
        assert!(events.iter().any(|e| matches!(
            e,
            WorkerEvent::Step(AgentEventKind::Message { text }) if text == "mcp result for: summarize"
        )));
        assert_eq!(session.result().text, "mcp result for: summarize");
    }

    /// A minimal context for the adapter tests (no root/ledger/approver needed —
    /// these adapters reach an external endpoint, not the local toolbox).
    fn ctx() -> WorkerContext {
        WorkerContext {
            root: None,
            snapshot_generation: 0,
            ledger: Arc::new(super::super::WorkerLedger::new()),
            approver: Arc::new(DenyApprover),
        }
    }

    struct DenyApprover;
    impl crate::delegate_proxy::DelegateApprover for DenyApprover {
        fn request(
            &self,
            _session_id: &str,
            _tool: &str,
            _args: &serde_json::Value,
            _tier: RiskTier,
            _preview: String,
            _cancel: &CancelToken,
        ) -> nerve_runtime::SessionApprovalDecision {
            nerve_runtime::SessionApprovalDecision::Deny
        }
    }
}
