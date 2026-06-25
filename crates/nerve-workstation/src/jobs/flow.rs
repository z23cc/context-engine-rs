//! Flow-engine (`flow.*`, C2) command executor for the [`JobManager`].
//!
//! `flow.start` runs the deterministic C1 orchestration engine as ONE cancellable
//! job; `flow.get` / `flow.list` / `flow.close` route through the live-flow registry;
//! `flow.respond` resolves a pending approval through the SAME approval hub
//! `session.respond` uses. The executors are `impl JobManager` methods so they reach
//! the manager's private runtime / registry / policy / launcher / flows registry.

use super::JobManager;
use crate::flow_job::{self, FlowDeps};
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{FlowSource, RuntimeCommand, SessionApprovalDecision};
use serde_json::Value;
use std::sync::Arc;

impl JobManager {
    /// Execute a `flow.*` command (C2). `flow.start` runs the deterministic C1 flow
    /// engine as ONE cancellable job (the `job_id` IS the `flow_id`), mapping the
    /// engine's worker events + node lifecycle onto the `flow_*` runtime events;
    /// `flow.get` / `flow.list` / `flow.close` route through the live-flow registry;
    /// `flow.respond` resolves a pending approval keyed by `flow_id` through the SAME
    /// [`ApprovalHub`](crate::session_manager::ApprovalHub) `session.respond` uses.
    pub(super) fn run_flow_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::FlowStart { workflow, .. } => {
                self.run_flow_start(job_id, workflow, token)
            }
            RuntimeCommand::FlowSteer {
                flow_id,
                target,
                message,
            } => self.run_flow_steer(&flow_id, &target, &message, token),
            RuntimeCommand::FlowReplay { ledger_ref, .. } => {
                self.run_flow_replay(job_id, ledger_ref, token)
            }
            RuntimeCommand::FlowGet { flow_id } => {
                let store =
                    crate::flow_store::FlowStore::for_scope(self.workspace_root().as_deref()).ok();
                flow_job::run_flow_get(&flow_id, &self.flows, store.as_ref())
            }
            RuntimeCommand::FlowList => {
                let store =
                    crate::flow_store::FlowStore::for_scope(self.workspace_root().as_deref()).ok();
                flow_job::run_flow_list(&self.flows, store.as_ref())
            }
            RuntimeCommand::FlowClose { flow_id } => {
                flow_job::run_flow_close(&flow_id, &self.flows)
            }
            RuntimeCommand::FlowRespond {
                flow_id,
                request_id,
                decision,
            } => self.run_flow_respond(&flow_id, &request_id, decision),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a flow.* command",
            )),
        }
    }

    /// Run a `flow.start` as a job: build the shared [`FlowDeps`] from the daemon's
    /// own runtime / registry / policy / launcher / approval hub, then drive the
    /// flow engine to completion ([`flow_job::run_flow_start`]). The `job_id` is the
    /// `flow_id`. CLI worker nodes require the `--allow-delegate` lift; provider
    /// worker nodes do not (mirrors the C1 `FlowArgs` gating).
    fn run_flow_start(
        &self,
        job_id: &str,
        workflow: FlowSource,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let deps = self.flow_deps();
        flow_job::run_flow_start(
            job_id,
            workflow,
            &deps,
            &self.flows,
            &self.emit,
            self.allow_delegate,
            token,
        )
    }

    /// Resolve a `flow.respond` through the shared approval hub keyed by `flow_id`.
    fn run_flow_respond(
        &self,
        flow_id: &str,
        request_id: &str,
        decision: SessionApprovalDecision,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        flow_job::run_flow_respond(flow_id, request_id, decision, &self.sessions.approvals())
    }

    /// Steer a live flow branch (C3a): look up the running flow's live-flow worker
    /// registry and run one more turn against the branch `target` selects, streaming
    /// the follow-up as `flow_node_agent` events scoped to `flow_id`. Runs as its own
    /// short job (`token` is that job's cancel) — distinct from the `flow.start` job.
    fn run_flow_steer(
        &self,
        flow_id: &str,
        target: &nerve_runtime::WorkerSelector,
        message: &str,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        flow_job::run_flow_steer(flow_id, target, message, &self.flows, &self.emit, token)
    }

    /// Assemble the [`FlowDeps`] the flow engine needs, all cloned from the daemon's
    /// own deps so a flow reaches tools through the SAME runtime, is gated by the
    /// SAME policy, and routes approvals through the SAME hub as sessions/delegation.
    /// The [`FlowStore`] is resolved per-call from the workspace root so a flow's tape
    /// persists under that project's `.nerve/flows` (C4); a rootless/unresolvable
    /// scope falls back to the global flows dir, and a resolve error disables
    /// persistence rather than failing the flow.
    fn flow_deps(&self) -> FlowDeps {
        let root = self.workspace_root();
        FlowDeps {
            runtime: Arc::clone(&self.runtime),
            registry: self.registry.clone(),
            policy: self.policy.clone(),
            delegate_launcher: Arc::clone(&self.delegate_launcher),
            approvals: self.sessions.approvals(),
            store: crate::flow_store::FlowStore::for_scope(root.as_deref()).ok(),
        }
    }

    /// The workspace root the daemon is scoped to (the first registered root), used
    /// to locate `.nerve/flows`. `None` when no root resolves.
    pub(super) fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.runtime
            .resolver()
            .resolve_workspace(None)
            .ok()
            .and_then(|ws| ws.roots().first().map(|r| r.path.clone()))
    }

    /// Run a `flow.replay` as a job (C4, design §3/§4): load the recorded ledger +
    /// def from the [`FlowStore`] and re-run the engine in REPLAY mode, re-emitting the
    /// `flow_*` event stream byte-identically at zero cost. The `job_id` is the
    /// replayed `flow_id`.
    fn run_flow_replay(
        &self,
        job_id: &str,
        ledger_ref: nerve_runtime::LedgerRef,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let deps = self.flow_deps();
        flow_job::run_flow_replay(job_id, ledger_ref, &deps, &self.flows, &self.emit, token)
    }
}
