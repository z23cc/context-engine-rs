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
use crate::flow::resolve::{failed_result, is_pipeline, provider_model, step_for_node};
use crate::flow::{FlowProgress, NodeId};
use crate::worker::{
    BudgetGrant, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerSession, WorkerSlot,
    WorkerTask,
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
        slot: Option<WorkerSlot>,
        wave_generation: u64,
        cancel: &CancelToken,
    ) -> NodeRun {
        // The process-global worker slot (C3b, the semaphore that bounds
        // `max_workers` tree-wide) was ALREADY acquired by `partition_by_budget` in
        // declared order — admission is deterministic, never decided by a threaded
        // `acquire()` race here (finding B). The slot rides along for this turn's
        // lifetime and is released when the run is folded.
        let Some(step_def) = step_for_node(def, node) else {
            return NodeRun::failed(&format!("no step for node `{node}`"));
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
        // The node's EFFECTIVE autonomy: its declared autonomy intersected with every
        // ancestor planner's, so a Hierarchical child can only NARROW its parent's
        // authority (finding H, design §6 monotone de-escalation). For a flat node this
        // is its own autonomy; for a `child/…` node it is `min(child, planner…)`.
        let autonomy = crate::flow::resolve::effective_autonomy(def, node);
        let task = self.build_task(step_def, def, node, step_index, autonomy);
        let ctx = self.worker_context(node, wave_generation);
        // Writer-node path-lease (C4, design §6): a writer (Edit/Full autonomy) holds
        // its scope's lease for the whole turn, so two writer-nodes on overlapping
        // scope SERIALIZE (never concurrent) — the safety property + the precondition
        // for replay fidelity under file mutation. A reader takes no lease. Keyed by the
        // EFFECTIVE autonomy, so a child clamped to ReadOnly takes no writer lease. The
        // `lease` Arc and its `_lease_guard` both live for the body of `run_node`
        // (across `worker.start`); the guard releases when this returns.
        let lease = self
            .leases
            .and_then(|leases| leases.lease_for(autonomy, self.root.as_deref()));
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
    /// - `{{split}}` — a `MapReduce`-only alias for THIS map node's deterministic split
    ///   item (the shard label or path group, design §3). Empty for a non-map node.
    ///
    /// An unknown/unresolved placeholder is left verbatim (the [`TaskTemplate`]
    /// contract — no silent emptying). Pure and deterministic: the same recorded
    /// blackboard renders byte-identically, so replay stays faithful.
    fn build_task(
        &self,
        step_def: &Step,
        def: &WorkflowDef,
        node: &NodeId,
        step_index: usize,
        autonomy: nerve_runtime::DelegateAutonomy,
    ) -> WorkerTask {
        let ledger = Arc::clone(&self.ledger);
        let prev_node = (is_pipeline(def) && step_index > 0).then(|| NodeId::stage(step_index - 1));
        let split = map_split_item(def, node);
        let prompt = step_def.task.render(&|name| {
            // `{{prev}}` is the upstream stage's output; `{{split}}` is this map node's
            // shard; everything else is a node id looked up directly in the blackboard.
            match name {
                "prev" => prev_node
                    .as_ref()
                    .and_then(|node| ledger.output(node.as_str())),
                "split" => split.clone(),
                other => ledger.output(other),
            }
        });
        WorkerTask {
            // The stable NodeId the engine assigned this dispatch — the REPLAY key
            // (so two distinct nodes that render an identical prompt never collide on
            // the tape) and the CLI approval-projection namespace.
            node_id: node.to_string(),
            prompt,
            // The EFFECTIVE autonomy (already intersected with every ancestor planner's
            // by `effective_autonomy`, finding H), so a Hierarchical child can only
            // narrow its parent's authority — never widen it.
            autonomy,
            model: provider_model(&step_def.worker),
            // Tool filter is the intersection with each ancestor planner's (monotone
            // de-escalation, finding H). A declared `Step` carries no per-step filter
            // today, so the intersection is vacuously `None` (∩ of empties); the
            // de-escalation seam is the `autonomy` clamp above, where authority lives.
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

    /// Build the [`WorkerContext`] for a node with the `wave_generation` already pinned
    /// ONCE for the whole wave on the engine thread (design §5, finding M). Pinning per
    /// wave — not per concurrent `run_node` — makes the recorded generation independent
    /// of thread timing: every branch in a parallel wave starts against the SAME
    /// snapshot, so a concurrent external/steer mutation can't make the recorded
    /// per-node generations depend on which branch read the live snapshot first. The CLI
    /// driver and tests pin `0` (a stable, file-mutation-free generation), keeping their
    /// replay byte-identical.
    fn worker_context(&self, node: &NodeId, wave_generation: u64) -> WorkerContext {
        WorkerContext {
            root: self.root.clone(),
            snapshot_generation: wave_generation,
            ledger: Arc::clone(&self.ledger),
            approver: Arc::clone(&self.approver),
            // Threaded so a CLI worker keys its approval by `flow_id` (finding F),
            // namespacing the projection by this node id.
            flow_id: self.flow_id.clone(),
            node_id: node.to_string(),
        }
    }

    /// Pin the snapshot generation for an entire wave ONCE, on the engine thread, before
    /// any branch spawns (design §5, finding M). A `generation` provider (the live
    /// snapshot, supplied by the host) is consulted a SINGLE time per wave — keyed by the
    /// wave's FIRST admitted node in declared order — so all branches in the wave record
    /// the same generation regardless of completion/thread timing. Without a provider the
    /// generation is `0` (the deterministic default for the CLI driver and tests). Pure
    /// given the provider — replay supplies a provider returning each node's RECORDED
    /// generation (all equal within a wave), so the pinned value matches the record run.
    pub(super) fn pin_wave_generation(&self, def: &WorkflowDef, first_node: &NodeId) -> u64 {
        match &self.generation {
            Some(provider) => provider(def, first_node.as_str()),
            None => 0,
        }
    }
}

/// The deterministic split item a `map-{i}` node of a `MapReduce` strategy sees as its
/// `{{split}}` (design §3), or `None` for any other node/strategy. A pure function of
/// the `WorkflowDef` + node id (the split is data, no filesystem walk), so a map worker
/// renders byte-identically on replay. A nested `child/map-{i}` node (a `Hierarchical`
/// whose child is a `MapReduce`) resolves against the child strategy by stripping the
/// `child/` prefix and recursing — mirroring [`step_in_strategy`](crate::flow::resolve)
/// — so a nested map node still finds its shard (finding L).
fn map_split_item(def: &WorkflowDef, node: &NodeId) -> Option<String> {
    split_item_in_strategy(&def.strategy, node.as_str())
}

/// Resolve a (possibly `child/`-prefixed) map node's split item against `strategy`,
/// descending into a `Hierarchical` child per stripped prefix until a `MapReduce` is
/// reached. Pure string decode, total over the closed enum.
fn split_item_in_strategy(strategy: &nerve_runtime::Strategy, node: &str) -> Option<String> {
    // A child-flow node delegates to the Hierarchical child strategy, prefix stripped.
    if let Some(inner) = node.strip_prefix(crate::flow::CHILD_PREFIX) {
        return match strategy {
            nerve_runtime::Strategy::Hierarchical { child, .. } => {
                split_item_in_strategy(child, inner)
            }
            _ => None,
        };
    }
    let nerve_runtime::Strategy::MapReduce { over, .. } = strategy else {
        return None;
    };
    let index: usize = node.strip_prefix("map-")?.parse().ok()?;
    crate::flow::split_item(over, index)
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
}
