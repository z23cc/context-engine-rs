//! Per-node execution for the [`Driver`](super::Driver) (split out of `driver/mod.rs`
//! for the file-size convention).
//!
//! This holds the node-level half of the driver: running one worker turn end-to-end
//! ([`Driver::run_node`]) — acquiring the budget slot + writer-node path-lease,
//! rendering the task from the ledger blackboard, pinning the per-node snapshot
//! generation, and starting the worker — plus the [`NodeRun`] / [`NodeResult`] folds
//! the wave-dispatch loop in [`super`] writes to the ledger in declared order. All
//! the nondeterminism a flow has lives here; the loop in [`super`] stays a pure fold.

use super::Driver;
use crate::flow::resolve::{failed_result, is_pipeline, provider_model, refused_result, step_for};
use crate::flow::{FlowProgress, NodeId};
use crate::worker::{
    BudgetGrant, SpawnRefusal, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerSession,
    WorkerSlot, WorkerTask,
};
use nerve_core::CancelToken;
use nerve_runtime::{Step, WorkflowDef};
use std::sync::Arc;

impl Driver<'_> {
    /// Run one node's worker turn end-to-end: resolve the worker, render its task
    /// from the ledger blackboard, start it (turn 1), and return its [`TurnResult`]
    /// together with the events it emitted (BUFFERED, not written to the ledger here
    /// — the caller writes them in declared order so the tape is deterministic) and
    /// the LIVE session (so the caller can register it as a steerable frontier or
    /// close it). The live progress sink still fires during the turn for real-time
    /// display. A start/turn error maps to a failed result with no live session, so a
    /// sibling branch is never aborted (mirrors `bounded_fan_out`'s per-task isolation).
    pub(super) fn run_node(
        &self,
        def: &WorkflowDef,
        node: &NodeId,
        step_index: usize,
        cancel: &CancelToken,
    ) -> NodeRun {
        // Acquire a process-global worker slot for this turn's lifetime (C3b, the
        // semaphore that bounds `max_workers` tree-wide). `partition_by_budget`
        // already deterministically admitted this node; the atomic acquire here is
        // the real bound — if a concurrent wave grabbed the last slot first, fold as
        // a recorded refusal rather than over-spawning.
        let slot = match &self.fleet {
            Some(fleet) => match fleet.acquire() {
                Ok(slot) => Some(slot),
                Err(refusal) => {
                    if let Some(observer) = self.observer {
                        observer.spawn_refused(node.as_str(), refusal);
                    }
                    return NodeRun::refused(refusal);
                }
            },
            None => None,
        };
        let Some(step_def) = step_for(def, step_index) else {
            return NodeRun::failed(&format!("no step at index {step_index}"));
        };
        let worker = match self.resolver.resolve(&step_def.worker) {
            Ok(worker) => worker,
            Err(err) => return NodeRun::failed(&format!("resolve failed: {err}")),
        };
        // A node whose worker resolved is genuinely starting: fire the lifecycle
        // observer (C2 maps this to FlowNodeStarted) with the declared WorkerRef.
        if let Some(observer) = self.observer {
            observer.node_started(node.as_str(), &step_def.worker);
        }
        let task = self.build_task(step_def, def, step_index);
        let ctx = self.worker_context(def, node);
        // Writer-node path-lease (C4, design §6): a writer (Edit/Full autonomy) holds
        // its scope's lease for the whole turn, so two writer-nodes on overlapping
        // scope SERIALIZE (never concurrent) — the safety property + the precondition
        // for replay fidelity under file mutation. A reader takes no lease. The
        // `lease` Arc and its `_lease_guard` both live for the body of `run_node`
        // (across `worker.start`); the guard releases when this returns.
        let lease = self
            .leases
            .and_then(|leases| leases.lease_for(step_def.autonomy, self.root.as_deref()));
        let _lease_guard = lease.as_ref().map(|scope| crate::sync::lock_recover(scope));
        // The node-start to record FIRST (the rendered prompt + pinned generation,
        // design §5): the worker is genuinely starting here, so this is recorded for
        // every start outcome (ok / cancelled / errored), making the tape a
        // self-contained, faithfully-pinned replay source.
        let start = Some((task.prompt.clone(), ctx.snapshot_generation));
        let node_id = node.clone();
        let on_progress = self.on_progress;
        let mut events = Vec::new();
        let mut on_event = |event: WorkerEvent| {
            // Stream live (out-of-order is fine for display), buffer for the
            // declared-order ledger write.
            if let Some(sink) = on_progress {
                sink(FlowProgress {
                    node: node_id.to_string(),
                    event: event.clone(),
                });
            }
            events.push(event);
        };
        match worker.start(&task, &ctx, cancel, &mut on_event) {
            Ok(session) => NodeRun {
                result: session.result(),
                events,
                session: Some(session),
                slot,
                start,
            },
            Err(WorkerError::Cancelled) => NodeRun {
                result: failed_result("cancelled"),
                events,
                session: None,
                slot,
                start,
            },
            Err(err) => NodeRun {
                result: failed_result(&err.to_string()),
                events,
                session: None,
                slot,
                start,
            },
        }
    }

    /// Render a step's task template (design §3 / §5, the cross-stage blackboard),
    /// interpolating named-output placeholders from the ledger, then build the
    /// [`WorkerTask`]. The supported placeholders are deliberately MINIMAL —
    /// named-output substitution only, NO expression language (design §12, open
    /// question 3):
    ///
    /// - `{{<node-id>}}` — the recorded output text of any finished node, by its
    ///   deterministic id (`{{node-0}}` for a `Single`, `{{stage-0}}` for a
    ///   pipeline's first stage, `{{branch-1}}` for a parallel branch).
    /// - `{{prev}}` — a `Pipeline`-only alias for the immediately-upstream stage's
    ///   output (`stage-{index-1}`). For a non-pipeline node, or stage 0, `prev`
    ///   resolves to nothing and is left as a verbatim `{{prev}}` placeholder.
    ///
    /// An unknown/unresolved placeholder is left verbatim (the [`TaskTemplate`]
    /// contract — no silent emptying). Pure and deterministic: the same recorded
    /// blackboard renders byte-identically, so replay stays faithful.
    fn build_task(&self, step_def: &Step, def: &WorkflowDef, step_index: usize) -> WorkerTask {
        let ledger = Arc::clone(&self.ledger);
        let prev_node = (is_pipeline(def) && step_index > 0).then(|| NodeId::stage(step_index - 1));
        let prompt = step_def.task.render(&|name| {
            // `{{prev}}` is the upstream stage's output; everything else is a node
            // id looked up directly in the blackboard.
            if name == "prev" {
                return prev_node
                    .as_ref()
                    .and_then(|node| ledger.output(node.as_str()));
            }
            ledger.output(name)
        });
        WorkerTask {
            prompt,
            autonomy: step_def.autonomy,
            model: provider_model(&step_def.worker),
            tool_filter: None,
            // Carve this node's grant from the fleet envelope (C3b, design §6): the
            // per-node ceiling is the remaining fleet budget, INTERSECTED so a node
            // can never out-spend the fleet (monotone de-escalation). An unbudgeted
            // flow yields a default (uncapped) grant — the C1/C2 behaviour.
            budget: self.node_grant(),
        }
    }

    /// Carve a node's [`BudgetGrant`] from the fleet's current remaining headroom
    /// (design §6): the node may spend at most what the fleet has left, intersected
    /// with a default (uncapped) ask — so the grant only ever NARROWS the fleet
    /// envelope. Replayable: the grant is a pure function of the recorded budget
    /// fold (which is itself replayable).
    fn node_grant(&self) -> BudgetGrant {
        match &self.budget {
            Some(budget) => BudgetGrant {
                max_cost_usd: budget.remaining_usd(),
                max_tokens: budget.remaining_tokens(),
            }
            .intersect(&BudgetGrant::default()),
            None => BudgetGrant::default(),
        }
    }

    /// Build the [`WorkerContext`] for a node, pinning the `snapshot_generation` at
    /// node-start (design §5, replay fidelity under file mutation). The generation is
    /// resolved from the optional `generation` provider — the host (flow job) supplies
    /// one backed by the live snapshot, so a node that mutated files makes a LATER
    /// node observe a different generation, recorded honestly into the ledger. The
    /// CLI driver and tests leave it unset, pinning `0` (a stable, file-mutation-free
    /// generation), which keeps their replay byte-identical.
    fn worker_context(&self, def: &WorkflowDef, node: &NodeId) -> WorkerContext {
        WorkerContext {
            root: self.root.clone(),
            snapshot_generation: self.pin_generation(def, node),
            ledger: Arc::clone(&self.ledger),
            approver: Arc::clone(&self.approver),
        }
    }

    /// Resolve the snapshot generation to pin for `node` at start. A `generation`
    /// provider (the live snapshot, supplied by the host) is consulted per node-start;
    /// without one the generation is `0` (the deterministic default for the CLI driver
    /// and tests). Pure given the provider — replay supplies a provider that returns
    /// the node's RECORDED generation, so the pinned value matches the record run.
    fn pin_generation(&self, def: &WorkflowDef, node: &NodeId) -> u64 {
        match &self.generation {
            Some(provider) => provider(def, node.as_str()),
            None => 0,
        }
    }
}

/// A node's produced result + buffered events, paired with its id so a fan-out wave
/// can fold each back by the right node. `bounded_fan_out` preserves input order, so
/// the vec of these comes back in declared branch order — the determinism invariant
/// (the ledger is then written in that order).
pub(super) struct NodeResult {
    pub(super) node: NodeId,
    pub(super) run: NodeRun,
}

/// What one [`Driver::run_node`] produced: the final [`TurnResult`], the BUFFERED
/// events (written to the ledger in declared order by the caller), the LIVE session
/// when the turn started (so the caller registers it as a steerable frontier or
/// closes it), and the held [`WorkerSlot`] (the process-global semaphore unit,
/// released when the run is folded). The session is dropped — and thus implicitly NOT
/// steerable — for any node whose start errored/cancelled.
pub(super) struct NodeRun {
    pub(super) result: TurnResult,
    pub(super) events: Vec<WorkerEvent>,
    pub(super) session: Option<Box<dyn WorkerSession>>,
    /// The held worker slot (`None` for an unbudgeted flow or a refused/cancelled run
    /// that never acquired one).
    pub(super) slot: Option<WorkerSlot>,
    /// The node-start to record FIRST (the rendered prompt + pinned snapshot
    /// generation, design §5) — `Some` only for a node that genuinely started a
    /// worker, so a resolve/refuse/cancel-before-start records no `Start` entry.
    pub(super) start: Option<(String, u64)>,
}

impl NodeRun {
    /// A failed run with no session (a resolve/start error or a panicked thread).
    pub(super) fn failed(reason: &str) -> Self {
        Self {
            result: failed_result(reason),
            events: Vec::new(),
            session: None,
            slot: None,
            start: None,
        }
    }

    /// A cancelled run with no session.
    pub(super) fn cancelled() -> Self {
        Self::failed("cancelled")
    }

    /// A run refused at a budget ceiling (design §8) — no session, no slot; folds as
    /// the recorded refusal result.
    pub(super) fn refused(refusal: SpawnRefusal) -> Self {
        Self {
            result: refused_result(refusal),
            events: Vec::new(),
            session: None,
            slot: None,
            start: None,
        }
    }
}
