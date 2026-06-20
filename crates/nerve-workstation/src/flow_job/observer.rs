//! Event projection for the `flow.*` job runner (split out of `flow_job/mod.rs`
//! for the file-size convention).
//!
//! The [`FlowEventObserver`] maps the C1 driver's node-lifecycle + budget callbacks
//! onto the additive `flow_*` / `budget_*` / `flow_decision` [`RuntimeEvent`]s, and
//! the small pure helpers translate worker/strategy/usage domain types onto their
//! protocol shapes. Reuses `AgentEventKind` verbatim (a node pane renders like a
//! session pane). No engine state — pure projection.

use super::EventEmitter;
use crate::flow::FlowObserver;
use crate::flow_store::FlowStore;
use crate::worker::{BudgetDecision, BudgetSnapshot, SpawnRefusal, TurnResult, WorkerLedger};
use nerve_runtime::{
    FlowDecisionKind, FlowNodeUsage, FlowWorkerKind, RuntimeEvent, Strategy, WorkerRef,
};
use std::sync::Arc;

/// Emit the DAG edges implied by `strategy` (design §4, `FlowEdge`). C2's two
/// strategies have a structural fan-out from the flow root to each node; a richer
/// strategy (pipeline) emits node→node edges from the engine in C3. The root is the
/// synthetic id `"flow"` so a client can anchor the graph.
pub(super) fn emit_strategy_edges(flow_id: &str, strategy: &Strategy, emit: &Arc<EventEmitter>) {
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
        Strategy::Pipeline { stages } => emit_pipeline_edges(flow_id, stages.len(), emit),
        Strategy::VoteJudge { candidates, .. } => {
            // flow → cand-i (fan-out), then every cand-i → judge (the adjudication).
            for index in 0..candidates.len() {
                let cand = format!("cand-{index}");
                emit(RuntimeEvent::flow_edge(flow_id, "flow", cand.clone()));
                emit(RuntimeEvent::flow_edge(flow_id, cand, "judge"));
            }
        }
        Strategy::MapReduce { over, .. } => {
            // flow → map-i (fan-out over the split), then every map-i → reduce.
            for index in 0..crate::flow::split_len(over) {
                let map = format!("map-{index}");
                emit(RuntimeEvent::flow_edge(flow_id, "flow", map.clone()));
                emit(RuntimeEvent::flow_edge(flow_id, map, "reduce"));
            }
        }
        Strategy::Debate { sides, rounds, .. } => {
            emit_debate_edges(flow_id, sides.len(), *rounds, emit);
        }
        // A `Hierarchical` flow's child DAG is data-dependent on the planner's run, so
        // its edges emit from the engine as nodes start (the planner edge is the only
        // static one).
        Strategy::Hierarchical { .. } => {
            emit(RuntimeEvent::flow_edge(flow_id, "flow", "planner"));
        }
        _ => {}
    }
}

/// Emit a `Debate`'s DAG edges (C5): `flow → side-s-round-0`, each round's sides chain
/// to the next round's, and the final round's sides → `judge`. Static (declared sides
/// × rounds), so the edges are known at `flow.start`.
fn emit_debate_edges(flow_id: &str, sides: usize, rounds: u32, emit: &Arc<EventEmitter>) {
    if sides == 0 || rounds == 0 {
        return;
    }
    for side in 0..sides {
        emit(RuntimeEvent::flow_edge(
            flow_id,
            "flow",
            format!("side-{side}-round-0"),
        ));
        for round in 1..rounds {
            emit(RuntimeEvent::flow_edge(
                flow_id,
                format!("side-{side}-round-{}", round - 1),
                format!("side-{side}-round-{round}"),
            ));
        }
        emit(RuntimeEvent::flow_edge(
            flow_id,
            format!("side-{side}-round-{}", rounds - 1),
            "judge",
        ));
    }
}

/// Emit a `Pipeline`'s chain edges (C3a): `flow → stage-0 → stage-1 → …`, so a
/// client renders the sequential DAG. The structure is static (declared stages),
/// so the edges are known at `flow.start`.
fn emit_pipeline_edges(flow_id: &str, stages: usize, emit: &Arc<EventEmitter>) {
    let mut from = "flow".to_string();
    for index in 0..stages {
        let to = format!("stage-{index}");
        emit(RuntimeEvent::flow_edge(flow_id, from.clone(), to.clone()));
        from = to;
    }
}

/// The node-lifecycle observer that maps the C1 driver's callbacks onto the
/// `flow_*` protocol events: `node_started` → [`RuntimeEvent::FlowNodeStarted`],
/// each worker `Step` → [`RuntimeEvent::FlowNodeAgent`], `node_finished` →
/// [`RuntimeEvent::FlowNodeFinished`]. The progress sink (per worker event) is
/// installed alongside so a `Step` becomes a `FlowNodeAgent` keyed by node id.
pub(super) struct FlowEventObserver {
    flow_id: String,
    emit: Arc<EventEmitter>,
    /// Node-boundary persistence (C4, design §5): when a [`FlowStore`] + the live
    /// `ledger` are attached, `node_finished` ATOMICALLY persists the current tape to
    /// `.nerve/flows/<flow_id>/ledger.jsonl` — the engine's natural checkpoint, so a
    /// crash mid-node never tears the record. `None` = in-memory only (C2/C3).
    persist: Option<(FlowStore, Arc<WorkerLedger>)>,
}

impl FlowEventObserver {
    pub(super) fn new(flow_id: String, emit: Arc<EventEmitter>) -> Self {
        Self {
            flow_id,
            emit,
            persist: None,
        }
    }

    /// Attach node-boundary ledger persistence over `store` + the live `ledger`.
    pub(super) fn with_persistence(mut self, store: FlowStore, ledger: Arc<WorkerLedger>) -> Self {
        self.persist = Some((store, ledger));
        self
    }

    /// Persist the live ledger to disk at a node boundary (best-effort: a write error
    /// is logged but never fails the flow — persistence is durability, not correctness).
    fn persist_ledger(&self) {
        if let Some((store, ledger)) = &self.persist
            && let Err(err) = store.write_ledger(&self.flow_id, ledger)
        {
            eprintln!(
                "warning: failed to persist flow `{}` ledger: {err}",
                self.flow_id
            );
        }
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
        // Node boundary: persist the recorded tape so far (C4, design §5). The
        // driver calls this in DECLARED order from its ledger-write loop, so what is
        // persisted is exactly the deterministic recorded tape up to this node.
        self.persist_ledger();
    }

    fn budget_debited(&self, snapshot: BudgetSnapshot, decision: BudgetDecision) {
        // Always surface the running totals after a debit (design §6).
        (self.emit)(RuntimeEvent::budget_update(
            self.flow_id.clone(),
            snapshot.spent_usd,
            snapshot.spent_tokens,
        ));
        match decision {
            BudgetDecision::Within => {}
            BudgetDecision::Warn { limit_usd } => {
                (self.emit)(RuntimeEvent::budget_warning(
                    self.flow_id.clone(),
                    snapshot.spent_usd,
                    limit_usd,
                ));
            }
            BudgetDecision::Exhausted => {
                // The audit trail: a flow-wide budget-exhausted decision (the engine
                // cooperatively cancels every branch via the CancelToken). Keyed by
                // the synthetic root `"flow"` since it is not node-local.
                (self.emit)(RuntimeEvent::flow_decision(
                    self.flow_id.clone(),
                    "flow",
                    FlowDecisionKind::BudgetExhausted,
                ));
            }
        }
    }

    fn spawn_refused(&self, node: &str, refusal: SpawnRefusal) {
        (self.emit)(RuntimeEvent::flow_decision(
            self.flow_id.clone(),
            node.to_string(),
            refusal_kind(refusal),
        ));
    }

    fn decision(&self, node: &str, kind: &FlowDecisionKind) {
        // A pure interpreter audit decision (C5: vote tally / judge pick / debate
        // round, plus the depth-ceiling refusal from a Hierarchical planner). Surface
        // it verbatim as a node-keyed `FlowDecision` (the audit trail, design §4/§6).
        (self.emit)(RuntimeEvent::flow_decision(
            self.flow_id.clone(),
            node.to_string(),
            kind.clone(),
        ));
    }
}

/// Map a [`SpawnRefusal`] (design §8) onto the protocol [`FlowDecisionKind`] the
/// audit trail records.
fn refusal_kind(refusal: SpawnRefusal) -> FlowDecisionKind {
    match refusal {
        SpawnRefusal::Depth { depth, max_depth } => {
            FlowDecisionKind::DepthCeiling { depth, max_depth }
        }
        SpawnRefusal::Workers {
            live_workers,
            max_workers,
        } => FlowDecisionKind::WorkerCeiling {
            live_workers,
            max_workers,
        },
        // A budget-exhausted spawn refusal is recorded as the budget-exhausted
        // decision (same audit kind the debit-overrun path emits).
        SpawnRefusal::Budget => FlowDecisionKind::BudgetExhausted,
    }
}

impl FlowEventObserver {
    /// Map one worker [`WorkerEvent`](crate::worker::WorkerEvent) onto a node-scoped
    /// `FlowNodeAgent` (reusing `AgentEventKind` verbatim) or drop it (a raw CLI
    /// `Progress` line / re-projected approval has no structured node-agent step).
    pub(super) fn on_progress(&self, progress: &crate::flow::FlowProgress) {
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
pub(super) fn strategy_label(strategy: &Strategy) -> &'static str {
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
