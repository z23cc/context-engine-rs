//! Daemon execution of the `flow.*` command family (Wave C2).
//!
//! This is the host-layer composition that runs the deterministic C1 flow engine
//! ([`crate::flow`]) as a cancellable daemon **job** over the additive runtime
//! protocol (agent-orchestration design §4). It is the flow analogue of
//! [`crate::jobs`]'s `delegate.*` / `session.*` execution: a `flow.start` job runs
//! one [`Driver`](crate::flow::Driver) to completion in its job thread, mapping the
//! engine's worker events + node lifecycle onto the `flow_*` [`RuntimeEvent`]s; the
//! `job_id` IS the `flow_id`. `flow.get` / `flow.list` / `flow.close` mirror the
//! session/job equivalents through a live-flow registry, and `flow.respond` reuses
//! the existing [`ApprovalHub`] round-trip keyed by `flow_id` — exactly the
//! delegate/session pattern, no new approval mechanism.
//!
//! Determinism: the engine is pure; this module is the (nondeterministic) host
//! boundary that mints workers, streams events, and tracks live flows. It adds
//! NOTHING to `nerve-core`.

use crate::flow::{Driver, FactoryResolver, FlowObserver, FlowOutcome};
use crate::policy::{Policy, ToolGate};
use crate::providers::ProviderRegistry;
use crate::sandbox::SandboxLauncher;
use crate::session_manager::{ApprovalHub, FlowDecisionMemory, FlowProtocolApprover};
use crate::subagent::DEFAULT_MAX_DEPTH;
use crate::tools::NerveRuntime;
use crate::worker::{TurnResult, WorkerFactory, WorkerLedger};
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{
    ApprovalMode, FlowNodeUsage, FlowRunOutcome, FlowSource, FlowWorkerKind, RuntimeError,
    RuntimeEvent, SessionApprovalDecision, Strategy, WorkerRef, WorkflowDef,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type EventEmitter = dyn Fn(RuntimeEvent) + Send + Sync + 'static;

/// The shared deps a flow needs to build its [`WorkerFactory`] + gate + approver.
/// Cloned from the [`JobManager`](crate::jobs)'s own deps so the flow engine reaches
/// tools through the SAME runtime and is gated by the SAME policy.
#[derive(Clone)]
pub(crate) struct FlowDeps {
    pub(crate) runtime: Arc<NerveRuntime>,
    pub(crate) registry: ProviderRegistry,
    pub(crate) policy: Policy,
    /// Trust-bound launcher for CLI workers (a refusing launcher unless the daemon
    /// was started with `--allow-delegate`, mirroring the delegate job path).
    pub(crate) delegate_launcher: Arc<dyn SandboxLauncher>,
    /// The shared approval hub `flow.respond` resolves against (the same hub
    /// `session.respond` / delegate proxying use).
    pub(crate) approvals: Arc<ApprovalHub>,
}

/// One live flow's registry entry: its cancel token (so `flow.close` can interrupt
/// the engine loop) plus a lightweight status snapshot for `flow.get` / `flow.list`.
struct FlowEntry {
    cancel: CancelToken,
    strategy_label: &'static str,
    name: String,
    created_seq: u64,
    /// Set once the flow finishes (the run thread records its terminal outcome).
    outcome: Option<FlowRunOutcome>,
}

/// The registry of flows the daemon knows about, keyed by `flow_id`. Live flows
/// carry an uncancelled token; finished flows retain a snapshot for `flow.get`
/// until pruned. A sibling of [`LiveSessions`](crate::delegate_live::LiveSessions),
/// but a flow runs synchronously in its job thread (no parking), so the registry
/// only needs the cancel handle + status, not a live driver.
#[derive(Default)]
pub(crate) struct LiveFlows {
    flows: Mutex<HashMap<String, FlowEntry>>,
    next_seq: Mutex<u64>,
}

impl LiveFlows {
    /// Register a starting flow under `flow_id`, returning the cancel token the run
    /// thread drives the engine under (and `flow.close` fires).
    fn register(&self, flow_id: &str, def: &WorkflowDef) -> CancelToken {
        let cancel = CancelToken::new();
        let created_seq = {
            let mut seq = crate::sync::lock_recover(&self.next_seq);
            *seq += 1;
            *seq
        };
        crate::sync::lock_recover(&self.flows).insert(
            flow_id.to_string(),
            FlowEntry {
                cancel: cancel.clone(),
                strategy_label: strategy_label(&def.strategy),
                name: def.name.clone(),
                created_seq,
                outcome: None,
            },
        );
        cancel
    }

    /// Record a flow's terminal outcome (the run thread calls this when the driver
    /// returns), so a later `flow.get` reflects the result.
    fn record_outcome(&self, flow_id: &str, outcome: FlowRunOutcome) {
        if let Some(entry) = crate::sync::lock_recover(&self.flows).get_mut(flow_id) {
            entry.outcome = Some(outcome);
        }
    }

    /// Snapshot one flow as JSON for `flow.get` (running vs. finished + its outcome).
    fn get(&self, flow_id: &str) -> Result<Value, RuntimeError> {
        crate::sync::lock_recover(&self.flows)
            .get(flow_id)
            .map(|entry| entry.snapshot(flow_id))
            .ok_or_else(|| RuntimeError::adapter(format!("no flow `{flow_id}`")))
    }

    /// List all known flows as JSON, in registration order, for `flow.list`.
    fn list(&self) -> Value {
        let flows = crate::sync::lock_recover(&self.flows);
        let mut entries: Vec<(&String, &FlowEntry)> = flows.iter().collect();
        entries.sort_by_key(|(_, entry)| entry.created_seq);
        let flows: Vec<Value> = entries
            .into_iter()
            .map(|(id, entry)| entry.snapshot(id))
            .collect();
        json!({ "flows": flows })
    }

    /// Request close of a live flow: fire its cancel token (the engine loop checks
    /// it each step and returns a cancelled outcome). Errors on an unknown id.
    fn close(&self, flow_id: &str) -> Result<Value, RuntimeError> {
        let flows = crate::sync::lock_recover(&self.flows);
        let entry = flows
            .get(flow_id)
            .ok_or_else(|| RuntimeError::adapter(format!("no flow `{flow_id}`")))?;
        entry.cancel.cancel();
        Ok(json!({ "flow_id": flow_id, "closed": true }))
    }
}

impl FlowEntry {
    fn snapshot(&self, flow_id: &str) -> Value {
        let status = if self.outcome.is_some() {
            "finished"
        } else {
            "running"
        };
        json!({
            "flow_id": flow_id,
            "name": self.name,
            "strategy": self.strategy_label,
            "status": status,
            "outcome": self.outcome,
        })
    }
}

/// Execute a `flow.start`: register the flow, emit `flow_started`, run the C1
/// engine to completion mapping its lifecycle onto `flow_*` events, then emit
/// `flow_completed` (or `flow_failed`) and return turn-summary JSON as the job
/// result. The `flow_id` is the job id. `cancel` is the job's own token; it is
/// linked into the registry's token so a `flow.close` OR a `runtime/jobs/cancel`
/// both stop the engine.
pub(crate) fn run_flow_start(
    flow_id: &str,
    workflow: FlowSource,
    deps: &FlowDeps,
    flows: &LiveFlows,
    emit: &Arc<EventEmitter>,
    allow_delegate: bool,
    cancel: &CancelToken,
) -> Result<Value, RuntimeError> {
    let def = resolve_workflow(workflow)?;
    let registry_cancel = flows.register(flow_id, &def);
    // The engine runs under a token that fires if EITHER the job's own cancel OR a
    // flow.close (the registry token) fires.
    let run_cancel = combined_cancel(cancel, &registry_cancel);
    // Provider-worker gate: `Ask` resolves through the shared ApprovalHub keyed by
    // this flow id (so a gated tool raises ONE ApprovalRequested answered by
    // flow.respond), under the run token so a close aborts a pending approval.
    let gate = deps.gate_for_flow(flow_id, &run_cancel);
    let factory = build_factory(deps, gate, allow_delegate);

    emit(RuntimeEvent::flow_started(flow_id, def.strategy.clone()));
    emit_strategy_edges(flow_id, &def.strategy, emit);

    let ledger = Arc::new(WorkerLedger::new());
    // CLI workers raise approvals through the same hub via `WorkerContext.approver`.
    let approver: Arc<dyn crate::delegate_proxy::DelegateApprover> =
        Arc::clone(&deps.approvals) as Arc<dyn crate::delegate_proxy::DelegateApprover>;
    let resolver = FactoryResolver::new(factory);
    let observer = FlowEventObserver::new(flow_id.to_string(), Arc::clone(emit));
    let root = deps
        .runtime
        .resolver()
        .resolve_workspace(None)
        .ok()
        .and_then(|ws| ws.roots().first().map(|r| r.path.clone()));

    // The progress sink maps each worker `Step` onto a node-scoped `FlowNodeAgent`;
    // the observer maps node start/finish onto `FlowNodeStarted`/`FlowNodeFinished`.
    let on_progress = |progress: crate::flow::FlowProgress| observer.on_progress(&progress);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, root)
        .with_observer(&observer)
        .with_progress(&on_progress);
    let outcome = driver.run(&def, &run_cancel);

    finish_flow(
        flow_id,
        &def,
        &outcome,
        flows,
        emit,
        cancel,
        &registry_cancel,
    )
}

/// Resolve the additive [`FlowSource`] into a concrete [`WorkflowDef`]. The inline
/// form is wired; a named ref is the P3 workflow-def loader surface (defined-ahead),
/// refused here with a clear message — the protocol shape is stable regardless.
fn resolve_workflow(workflow: FlowSource) -> Result<WorkflowDef, RuntimeError> {
    match workflow {
        FlowSource::Inline { workflow } => Ok(*workflow),
        FlowSource::Named { workflow_ref } => Err(RuntimeError::adapter(format!(
            "named workflow `{workflow_ref}` is not yet resolvable (inline `workflow` is supported; \
             named workflow-def loading lands with the P3 workflow-defs loader)"
        ))),
    }
}

/// Build the [`WorkerFactory`] over the flow's deps + the flow-scoped `gate`. CLI
/// workers run under the trust-bound delegate launcher (refusing unless
/// `--allow-delegate`); provider workers reach tools through the shared runtime
/// behind `gate` (whose `Ask` routes to the `ApprovalHub` keyed by the flow id).
fn build_factory(deps: &FlowDeps, gate: ToolGate, allow_delegate: bool) -> WorkerFactory {
    let delegate_launcher = if allow_delegate {
        Arc::clone(&deps.delegate_launcher)
    } else {
        crate::sandbox::refuse_launcher()
    };
    WorkerFactory::new(
        delegate_launcher,
        Arc::clone(&deps.runtime),
        deps.registry.clone(),
        gate,
        DEFAULT_MAX_DEPTH,
    )
}

impl FlowDeps {
    /// Build a provider-worker gate whose `Ask` decisions route through the shared
    /// [`ApprovalHub`] keyed by `flow_id`, with per-flow decision memory.
    fn gate_for_flow(&self, flow_id: &str, cancel: &CancelToken) -> ToolGate {
        let memory: FlowDecisionMemory = Arc::new(Mutex::new(HashMap::new()));
        let approver = Arc::new(FlowProtocolApprover::new(
            flow_id.to_string(),
            Arc::clone(&self.approvals),
            cancel.clone(),
            memory,
        ));
        ToolGate::with_approver(self.policy.clone(), ApprovalMode::AlwaysAsk, approver)
    }
}

/// Resolve a `flow.get`. Mirrors `session.get` / `runtime/jobs/get`.
pub(crate) fn run_flow_get(flow_id: &str, flows: &LiveFlows) -> Result<Value, RuntimeError> {
    flows.get(flow_id).map(|flow| json!({ "flow": flow }))
}

/// Resolve a `flow.list`. Mirrors `session.list`.
pub(crate) fn run_flow_list(flows: &LiveFlows) -> Result<Value, RuntimeError> {
    Ok(flows.list())
}

/// Resolve a `flow.close`. Mirrors `session.close` / `delegate.close`.
pub(crate) fn run_flow_close(flow_id: &str, flows: &LiveFlows) -> Result<Value, RuntimeError> {
    flows.close(flow_id)
}

/// Resolve a `flow.respond`: route the decision through the shared [`ApprovalHub`]
/// keyed by `flow_id` (a flow branch is just another approval id). Reuses the SAME
/// round-trip `session.respond` / delegate approvals use — no new mechanism.
pub(crate) fn run_flow_respond(
    flow_id: &str,
    request_id: &str,
    decision: SessionApprovalDecision,
    approvals: &ApprovalHub,
) -> Result<Value, RuntimeError> {
    let responded = approvals.respond(flow_id, request_id, decision);
    Ok(json!({ "flow_id": flow_id, "request_id": request_id, "responded": responded }))
}

/// Emit the terminal `flow_completed` / `flow_failed` event, record the outcome in
/// the registry, and return the job-result JSON. A cancelled flow (the job token or
/// a `flow.close` fired) maps to a cancelled [`RuntimeError`] so the job finishes
/// `job_cancelled`, not `job_failed`.
fn finish_flow(
    flow_id: &str,
    def: &WorkflowDef,
    outcome: &FlowOutcome,
    flows: &LiveFlows,
    emit: &Arc<EventEmitter>,
    job_cancel: &CancelToken,
    registry_cancel: &CancelToken,
) -> Result<Value, RuntimeError> {
    let cancelled = job_cancel.is_cancelled() || registry_cancel.is_cancelled();
    let run_outcome = FlowRunOutcome {
        ok: outcome.ok,
        summary: outcome.summary.clone(),
        final_text: outcome.final_text(),
    };
    flows.record_outcome(flow_id, run_outcome.clone());
    if cancelled {
        emit(RuntimeEvent::flow_failed(
            flow_id,
            None,
            "flow cancelled".to_string(),
        ));
        return Err(RuntimeError::cancelled());
    }
    if outcome.ok {
        emit(RuntimeEvent::flow_completed(flow_id, run_outcome.clone()));
    } else {
        emit(RuntimeEvent::flow_failed(
            flow_id,
            None,
            outcome.summary.clone(),
        ));
    }
    Ok(json!({
        "flow_id": flow_id,
        "name": def.name,
        "ok": outcome.ok,
        "summary": outcome.summary,
        "final_text": run_outcome.final_text,
    }))
}

/// Emit the DAG edges implied by `strategy` (design §4, `FlowEdge`). C2's two
/// strategies have a structural fan-out from the flow root to each node; a richer
/// strategy (pipeline) emits node→node edges from the engine in C3. The root is the
/// synthetic id `"flow"` so a client can anchor the graph.
fn emit_strategy_edges(flow_id: &str, strategy: &Strategy, emit: &Arc<EventEmitter>) {
    match strategy {
        Strategy::Single { .. } => {
            emit(RuntimeEvent::flow_edge(flow_id, "flow", "node-0"));
        }
        Strategy::Parallel { branches, .. } => {
            for index in 0..branches.len() {
                emit(RuntimeEvent::flow_edge(
                    flow_id,
                    "flow",
                    format!("branch-{index}"),
                ));
            }
        }
        // Defined-ahead strategies emit their edges from the engine in C3/C5.
        _ => {}
    }
}

/// The node-lifecycle observer that maps the C1 driver's callbacks onto the
/// `flow_*` protocol events: `node_started` → [`RuntimeEvent::FlowNodeStarted`],
/// each worker `Step` → [`RuntimeEvent::FlowNodeAgent`], `node_finished` →
/// [`RuntimeEvent::FlowNodeFinished`]. The progress sink (per worker event) is
/// installed alongside so a `Step` becomes a `FlowNodeAgent` keyed by node id.
struct FlowEventObserver {
    flow_id: String,
    emit: Arc<EventEmitter>,
}

impl FlowEventObserver {
    fn new(flow_id: String, emit: Arc<EventEmitter>) -> Self {
        Self { flow_id, emit }
    }
}

impl FlowObserver for FlowEventObserver {
    fn node_started(&self, node: &str, worker: &WorkerRef) {
        let (label, kind) = worker_label(worker);
        (self.emit)(RuntimeEvent::flow_node_started(
            self.flow_id.clone(),
            node.to_string(),
            label,
            kind,
        ));
    }

    fn node_finished(&self, node: &str, result: &TurnResult) {
        (self.emit)(RuntimeEvent::flow_node_finished(
            self.flow_id.clone(),
            node.to_string(),
            result.ok,
            usage_to_flow(&result.usage),
        ));
    }
}

impl FlowEventObserver {
    /// Map one worker [`WorkerEvent`](crate::worker::WorkerEvent) onto a node-scoped
    /// `FlowNodeAgent` (reusing `AgentEventKind` verbatim) or drop it (a raw CLI
    /// `Progress` line / re-projected approval has no structured node-agent step).
    fn on_progress(&self, progress: &crate::flow::FlowProgress) {
        if let crate::worker::WorkerEvent::Step(kind) = &progress.event {
            (self.emit)(RuntimeEvent::flow_node_agent(
                self.flow_id.clone(),
                progress.node.clone(),
                kind.clone(),
            ));
        }
    }
}

/// A human-readable worker label + its [`FlowWorkerKind`] family for
/// `FlowNodeStarted`.
fn worker_label(worker: &WorkerRef) -> (String, FlowWorkerKind) {
    match worker {
        WorkerRef::Cli { name } => (name.clone(), FlowWorkerKind::Cli),
        WorkerRef::Provider { provider, model } => {
            (format!("{provider}/{model}"), FlowWorkerKind::Provider)
        }
        WorkerRef::Named { name } => (name.clone(), FlowWorkerKind::Provider),
    }
}

/// Map a [`nerve_agent::Usage`] onto the protocol [`FlowNodeUsage`], omitting zero
/// cache counts (matching the agent-event discipline).
fn usage_to_flow(usage: &nerve_agent::Usage) -> FlowNodeUsage {
    FlowNodeUsage {
        input_tokens: u64::from(usage.input_tokens),
        output_tokens: u64::from(usage.output_tokens),
        cache_read_tokens: (usage.cache_read_tokens > 0)
            .then(|| u64::from(usage.cache_read_tokens)),
        cache_creation_tokens: (usage.cache_creation_tokens > 0)
            .then(|| u64::from(usage.cache_creation_tokens)),
    }
}

/// A stable label for a strategy (registry status + edge derivation).
fn strategy_label(strategy: &Strategy) -> &'static str {
    match strategy {
        Strategy::Single { .. } => "single",
        Strategy::Parallel { .. } => "parallel",
        Strategy::Pipeline { .. } => "pipeline",
        Strategy::MapReduce { .. } => "map_reduce",
        Strategy::VoteJudge { .. } => "vote_judge",
        Strategy::Debate { .. } => "debate",
        Strategy::Hierarchical { .. } => "hierarchical",
        _ => "unknown",
    }
}

/// Combine the job's own cancel token and the registry's flow-close token into one
/// token the engine polls, so either source stops the flow. A tiny watcher fans
/// both into the combined token (the engine loop only checks `is_cancelled()`).
fn combined_cancel(job: &CancelToken, registry: &CancelToken) -> CancelToken {
    let combined = CancelToken::new();
    if job.is_cancelled() || registry.is_cancelled() {
        combined.cancel();
        return combined;
    }
    let out = combined.clone();
    let job = job.clone();
    let registry = registry.clone();
    std::thread::spawn(move || {
        loop {
            if job.is_cancelled() || registry.is_cancelled() {
                out.cancel();
                return;
            }
            if out.is_cancelled() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    combined
}
