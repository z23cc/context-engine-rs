//! The driver — the only thing that applies [`Action`]s (design §3).
//!
//! The [`Driver`] runs the loop: call the pure [`engine::step`], apply the
//! returned [`Action`]s, then re-run `step` until [`Action::Terminate`]. Applying
//! a `StartWorker` mints a worker through the C0 [`WorkerFactory`], runs its turn,
//! and records every [`WorkerEvent`] + the final [`TurnResult`] into the
//! [`WorkerLedger`] — the only place nondeterminism enters. A `Parallel` wave is
//! dispatched through the **already-built** [`bounded_fan_out`], REUSED verbatim
//! (it preserves INPUT ORDER, the invariant behind the declared-order fold).

use super::engine::{FlowState, step};
use super::resolve::{
    WorkerResolver, cancelled_outcome, failed_result, is_pipeline, is_steerable_strategy,
    provider_model, refused_result, step_for, terminated_without_emit,
};
use super::{Action, FlowOutcome, NodeId};
use crate::subagent::bounded_fan_out;
use crate::worker::{
    BudgetDecision, BudgetGrant, BudgetLedger, BudgetSnapshot, FleetBudget, SpawnRefusal,
    SteerRegistry, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerLedger,
    WorkerSession, WorkerSlot, WorkerTask,
};
use nerve_core::CancelToken;
use nerve_runtime::{Step, WorkerRef, WorkflowDef};
use std::sync::Arc;

/// One `StartWorker` instruction the driver dispatches: the node + its flat step
/// index into the strategy's step list.
type Dispatch = (NodeId, usize);

/// A budget partition of one wave (design §8): the spawns the [`FleetBudget`]
/// admits, and the ones it refuses at a ceiling (with the typed [`SpawnRefusal`]).
type BudgetPartition = (Vec<Dispatch>, Vec<(NodeId, SpawnRefusal)>);

/// Default concurrency for a `Parallel` wave — mirrors `subagent`'s
/// `DEFAULT_FANOUT_CONCURRENCY`. ureq is synchronous, so each in-flight worker
/// occupies one OS thread; this caps that pressure. (C3 replaces it with the
/// `BudgetSpec::max_workers` process-global semaphore.)
const DEFAULT_FLOW_CONCURRENCY: usize = 4;

/// A streamed progress line from the engine: which node produced it and the
/// underlying [`WorkerEvent`]. The hidden CLI renders these; the C2
/// `FlowNodeAgent` protocol event carries the same `(node, event)` pair.
#[derive(Debug, Clone)]
pub(crate) struct FlowProgress {
    pub(crate) node: String,
    pub(crate) event: WorkerEvent,
}

/// Node-lifecycle observer the driver fires so a host (C2's flow job) can map the
/// run onto `flow_*` protocol events without parsing the progress stream. Additive
/// over C1's `on_progress` (which still fires per worker event): C1's hidden CLI
/// sets no observer; C2's flow job installs one that emits
/// [`RuntimeEvent::FlowNodeStarted`]/[`FlowNodeFinished`]. All callbacks fire from
/// the driver's own threads, so an implementor must be `Sync`.
pub(crate) trait FlowObserver: Sync {
    /// A node's worker is about to start. `worker` is the declared [`WorkerRef`].
    fn node_started(&self, node: &str, worker: &WorkerRef);
    /// A node's worker finished, with its recorded [`TurnResult`]. Fired in
    /// declared order (from the ledger-write loop), so two parallel branches'
    /// `node_finished` callbacks are ordered by branch index, not completion.
    fn node_finished(&self, node: &str, result: &TurnResult);
    /// The running budget totals after a node's usage was debited (design §6).
    /// `decision` says whether this debit warned or exhausted the budget; the host
    /// maps it onto `BudgetUpdate` / `BudgetWarning` / `FlowDecision` events.
    /// Default no-op so the hidden CLI (which sets no budget) need not implement it.
    fn budget_debited(&self, _snapshot: BudgetSnapshot, _decision: BudgetDecision) {}
    /// The engine refused to spawn `node` at a ceiling (design §8, absence-at-floor).
    /// The host records a `FlowDecision`. Default no-op for the hidden CLI.
    fn spawn_refused(&self, _node: &str, _refusal: SpawnRefusal) {}
}

/// The orchestration driver. Owns the shared [`WorkerContext`] deps (root /
/// ledger / approver), the resolver, and a progress sink. Drives one
/// [`WorkflowDef`] to a [`FlowOutcome`].
pub(crate) struct Driver<'a> {
    resolver: &'a dyn WorkerResolver,
    ledger: Arc<WorkerLedger>,
    approver: Arc<dyn crate::delegate_proxy::DelegateApprover>,
    root: Option<std::path::PathBuf>,
    /// Per-wave concurrency for `Parallel` (defaults to [`DEFAULT_FLOW_CONCURRENCY`]).
    concurrency: usize,
    /// Optional progress sink: each `(node, event)` pair as it is recorded.
    on_progress: Option<&'a (dyn Fn(FlowProgress) + Sync)>,
    /// Optional node-lifecycle observer (C2): node start/finish callbacks the flow
    /// job maps onto `flow_*` protocol events.
    observer: Option<&'a dyn FlowObserver>,
    /// Optional live-flow worker registry (C3a): for steerable single-node waves
    /// (`Single` / `Pipeline` stages), the driver keeps each frontier's live
    /// session registered here so a concurrent `flow.steer` can run a follow-up
    /// turn against it. A `Parallel` wave never registers (no single live frontier).
    steer: Option<&'a SteerRegistry>,
    /// Per-flow budget governance (C3b, design §6/§8). The [`BudgetLedger`] is a
    /// pure fold over each finished node's recorded usage (debited in the
    /// declared-order ledger-write loop, so it is replayable); the [`FleetBudget`]
    /// gates each spawn (depth / process-global worker semaphore / remaining
    /// budget — absence-at-floor). `None` = unbudgeted (the C1/C2/C3a behaviour),
    /// so existing flow tests stay green.
    budget: Option<Arc<BudgetLedger>>,
    fleet: Option<FleetBudget>,
}

impl<'a> Driver<'a> {
    /// Build a driver over the shared deps.
    pub(crate) fn new(
        resolver: &'a dyn WorkerResolver,
        ledger: Arc<WorkerLedger>,
        approver: Arc<dyn crate::delegate_proxy::DelegateApprover>,
        root: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            resolver,
            ledger,
            approver,
            root,
            concurrency: DEFAULT_FLOW_CONCURRENCY,
            on_progress: None,
            observer: None,
            steer: None,
            budget: None,
            fleet: None,
        }
    }

    /// Attach a progress sink that observes every recorded `(node, event)`.
    #[must_use]
    pub(crate) fn with_progress(mut self, sink: &'a (dyn Fn(FlowProgress) + Sync)) -> Self {
        self.on_progress = Some(sink);
        self
    }

    /// Attach a node-lifecycle [`FlowObserver`] (C2): the flow job maps its
    /// `node_started`/`node_finished` callbacks onto `flow_*` protocol events.
    #[must_use]
    pub(crate) fn with_observer(mut self, observer: &'a dyn FlowObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    /// Override the per-wave concurrency (tests pin it for determinism).
    #[must_use]
    pub(crate) fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Attach a live-flow [`SteerRegistry`] (C3a): the driver keeps each steerable
    /// single-node frontier's live session registered here so a concurrent
    /// `flow.steer` can run a follow-up turn against it.
    #[must_use]
    pub(crate) fn with_steer_registry(mut self, steer: &'a SteerRegistry) -> Self {
        self.steer = Some(steer);
        self
    }

    /// Attach per-flow budget governance (C3b, design §6/§8): the shared
    /// [`BudgetLedger`] (debited from each node's recorded usage, replayable) and
    /// the root [`FleetBudget`] (gating each spawn — depth / worker semaphore /
    /// remaining budget). Without this, the flow is unbudgeted (current behaviour).
    #[must_use]
    pub(crate) fn with_budget(mut self, budget: Arc<BudgetLedger>, fleet: FleetBudget) -> Self {
        self.budget = Some(budget);
        self.fleet = Some(fleet);
        self
    }

    /// Run `def` to completion: loop `step` → apply actions → record results,
    /// until `Terminate`. Returns the emitted [`FlowOutcome`] (or a terminal
    /// outcome if the flow never emitted one — e.g. it was cancelled). Always tears
    /// down any live steerable frontier on the way out (every exit path), so a
    /// steered session never outlives its flow.
    pub(crate) fn run(&self, def: &WorkflowDef, cancel: &CancelToken) -> FlowOutcome {
        let outcome = self.run_loop(def, cancel);
        if let Some(registry) = self.steer {
            registry.close_all();
        }
        outcome
    }

    /// The engine loop proper (see [`Self::run`], which wraps this with frontier
    /// teardown).
    fn run_loop(&self, def: &WorkflowDef, cancel: &CancelToken) -> FlowOutcome {
        let mut state = FlowState::new();
        let mut emitted: Option<FlowOutcome> = None;
        // Bound the loop defensively: each iteration must make progress (dispatch
        // at least one node or terminate), so the step count is bounded by the
        // node count; the cap is a safety net against an interpreter bug.
        for _ in 0..MAX_STEPS {
            if cancel.is_cancelled() {
                return cancelled_outcome();
            }
            let actions = step(&state, def);
            if actions.is_empty() {
                // No actions and not terminated means the interpreter is waiting
                // on results it should already have — a bug. Break to the fallback.
                break;
            }
            let mut starts: Vec<(NodeId, usize)> = Vec::new();
            for action in actions {
                match action {
                    Action::StartWorker { node, step_index } => starts.push((node, step_index)),
                    Action::Emit { outcome } => emitted = Some(outcome),
                    Action::Terminate => {
                        return emitted.unwrap_or_else(terminated_without_emit);
                    }
                    // The interpreter never emits these (declared-ahead, C3+).
                    Action::SteerWorker { .. }
                    | Action::CloseWorker { .. }
                    | Action::RequestApproval { .. } => {}
                }
            }
            if !starts.is_empty() {
                self.dispatch_wave(def, &starts, &mut state, cancel);
            }
        }
        emitted.unwrap_or_else(terminated_without_emit)
    }

    /// Dispatch one wave of `StartWorker`s and fold their results back into
    /// `state`. A single start runs inline; a multi-node wave runs through
    /// [`bounded_fan_out`] (REUSED verbatim — input order preserved), so results
    /// map 1:1 to declared branch order regardless of completion order.
    fn dispatch_wave(
        &self,
        def: &WorkflowDef,
        starts: &[(NodeId, usize)],
        state: &mut FlowState,
        cancel: &CancelToken,
    ) {
        // Mark every node dispatched first, so a re-`step` after a partial fold
        // never re-dispatches an in-flight node.
        for (node, _) in starts {
            state.mark_dispatched(node.clone());
        }
        // Budget gate (C3b, design §8): partition the wave into spawns the
        // FleetBudget admits and spawns it refuses at a ceiling (absence-at-floor).
        // A refused node is NOT spawned; it is recorded as a deterministic
        // FlowDecision and folded as a failure result so the engine still
        // terminates (the interpreter sees a recorded result for the node).
        let (admitted, refused) = self.partition_by_budget(starts);
        for (node, refusal) in &refused {
            if let Some(observer) = self.observer {
                observer.spawn_refused(node.as_str(), *refusal);
            }
            let result = refused_result(*refusal);
            self.ledger.record_result(node.as_str(), &result);
            state.record_result(node.clone(), result);
        }
        if admitted.is_empty() {
            return;
        }
        // A single-node wave on a steerable strategy (`Single`/`Pipeline`) is the
        // flow's current frontier: its live session is kept registered so a
        // concurrent `flow.steer` can run a follow-up turn (C3a). A `Parallel` wave
        // (multi-node) has no single live frontier and never registers.
        let steerable = admitted.len() == 1 && is_steerable_strategy(&def.strategy);
        let inputs: Vec<(NodeId, usize)> = admitted;
        let results = bounded_fan_out(
            inputs,
            self.concurrency,
            cancel,
            |(node, step_index)| {
                let run = self.run_node(def, &node, step_index, cancel);
                NodeResult { node, run }
            },
            |(node, _)| NodeResult {
                node: node.clone(),
                run: NodeRun::cancelled(),
            },
            || NodeResult {
                // A worker thread that panicked has no node id to recover; the
                // node was still marked dispatched, so it folds as a failure.
                node: NodeId::single(),
                run: NodeRun::failed("worker thread panicked"),
            },
        );
        // bounded_fan_out preserves INPUT ORDER, so `results` is in declared
        // branch order. Writing the ledger here (in declared order) — NOT inside
        // the concurrent worker closures — is what makes the replay tape a
        // deterministic function of declared order, regardless of which branch
        // finished first (design §3, the determinism invariant that the
        // byte-identical replay gate enforces).
        for node_result in results {
            let NodeResult { node, run } = node_result;
            let NodeRun {
                result,
                events,
                session,
                slot,
            } = run;
            for event in events {
                self.ledger.record_event(node.as_str(), event);
            }
            self.ledger.record_result(node.as_str(), &result);
            // Fire the lifecycle observer in declared order (this loop is the
            // declared-order ledger write), so two parallel branches' finishes are
            // ordered by branch index, not completion — C2 maps this to
            // FlowNodeFinished.
            if let Some(observer) = self.observer {
                observer.node_finished(node.as_str(), &result);
            }
            // Debit the budget from this node's RECORDED result, in the same
            // declared-order loop (C3b, design §6) — so the running totals are a
            // pure fold over recorded usage and replay reproduces them
            // byte-identically. On overrun, cooperatively cancel the run.
            self.debit_budget(&result, cancel);
            self.handle_live_session(steerable, node.as_str(), session);
            // The live-worker slot releases here (after the turn folded), so the
            // process-global semaphore frees up for the next wave.
            drop(slot);
            state.record_result(node, result);
        }
    }

    /// Partition a wave's `starts` into the spawns the [`FleetBudget`] admits and
    /// those it refuses at a ceiling (depth / worker / budget — absence-at-floor,
    /// design §8). Unbudgeted flows (no [`FleetBudget`]) admit everything, so the
    /// C1/C2/C3a behaviour is unchanged.
    fn partition_by_budget(&self, starts: &[Dispatch]) -> BudgetPartition {
        let Some(fleet) = &self.fleet else {
            return (starts.to_vec(), Vec::new());
        };
        let mut admitted = Vec::new();
        let mut refused = Vec::new();
        for (node, step_index) in starts {
            match fleet.may_spawn() {
                Ok(()) => admitted.push((node.clone(), *step_index)),
                Err(refusal) => refused.push((node.clone(), refusal)),
            }
        }
        (admitted, refused)
    }

    /// Debit one finished node's recorded result into the [`BudgetLedger`] (a pure
    /// fold), fire the budget observer, and — on a hard overrun — cooperatively
    /// cancel the run (the same `CancelToken` mechanism `CostTelemetryHook` uses),
    /// so every other branch stops at its next cancellation check. A no-op for an
    /// unbudgeted flow.
    fn debit_budget(&self, result: &TurnResult, cancel: &CancelToken) {
        let Some(budget) = &self.budget else {
            return;
        };
        let decision = budget.debit(result);
        if let Some(observer) = self.observer {
            observer.budget_debited(budget.snapshot(), decision);
        }
        if matches!(decision, BudgetDecision::Exhausted) {
            cancel.cancel();
        }
    }

    /// Decide what to do with a finished node's live session: register it as the
    /// new steerable frontier (closing the previous one) when the wave is steerable
    /// and a registry is attached, otherwise close it immediately. Without a
    /// registry, behaviour is identical to C2 (close right after the turn).
    fn handle_live_session(
        &self,
        steerable: bool,
        node: &str,
        session: Option<Box<dyn WorkerSession>>,
    ) {
        let Some(mut session) = session else {
            return;
        };
        match (steerable, self.steer) {
            (true, Some(registry)) => {
                // Advance the frontier: close every prior frontier, then register
                // this node so it is the single live, steerable worker.
                registry.close_all();
                registry.register(node, session);
            }
            _ => session.close(),
        }
    }

    /// Run one node's worker turn end-to-end: resolve the worker, render its task
    /// from the ledger blackboard, start it (turn 1), and return its
    /// [`TurnResult`] together with the events it emitted (BUFFERED, not written
    /// to the ledger here — the caller writes them in declared order so the tape
    /// is deterministic) and the LIVE session (so the caller can register it as a
    /// steerable frontier or close it). The live progress sink still fires during
    /// the turn for real-time display. A start/turn error maps to a failed result
    /// with no live session, so a sibling branch is never aborted (mirrors
    /// `bounded_fan_out`'s per-task isolation).
    fn run_node(
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
        let ctx = self.worker_context();
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
            },
            Err(WorkerError::Cancelled) => NodeRun {
                result: failed_result("cancelled"),
                events,
                session: None,
                slot,
            },
            Err(err) => NodeRun {
                result: failed_result(&err.to_string()),
                events,
                session: None,
                slot,
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

    fn worker_context(&self) -> WorkerContext {
        WorkerContext {
            root: self.root.clone(),
            snapshot_generation: 0,
            ledger: Arc::clone(&self.ledger),
            approver: Arc::clone(&self.approver),
        }
    }
}

/// A node's produced result + buffered events, paired with its id so a fan-out
/// wave can fold each back by the right node. `bounded_fan_out` preserves input
/// order, so the vec of these comes back in declared branch order — the
/// determinism invariant (the ledger is then written in that order).
struct NodeResult {
    node: NodeId,
    run: NodeRun,
}

/// What one [`Driver::run_node`] produced: the final [`TurnResult`], the BUFFERED
/// events (written to the ledger in declared order by the caller), the LIVE
/// session when the turn started (so the caller registers it as a steerable
/// frontier or closes it), and the held [`WorkerSlot`] (the process-global
/// semaphore unit, released when the run is folded). The session is dropped — and
/// thus implicitly NOT steerable — for any node whose start errored/cancelled.
struct NodeRun {
    result: TurnResult,
    events: Vec<WorkerEvent>,
    session: Option<Box<dyn WorkerSession>>,
    /// The held worker slot (`None` for an unbudgeted flow or a refused/cancelled
    /// run that never acquired one).
    slot: Option<WorkerSlot>,
}

impl NodeRun {
    /// A failed run with no session (a resolve/start error or a panicked thread).
    fn failed(reason: &str) -> Self {
        Self {
            result: failed_result(reason),
            events: Vec::new(),
            session: None,
            slot: None,
        }
    }

    /// A cancelled run with no session.
    fn cancelled() -> Self {
        Self::failed("cancelled")
    }

    /// A run refused at a budget ceiling (design §8) — no session, no slot; folds
    /// as the recorded refusal result.
    fn refused(refusal: SpawnRefusal) -> Self {
        Self {
            result: refused_result(refusal),
            events: Vec::new(),
            session: None,
            slot: None,
        }
    }
}

/// Safety net against an interpreter that never terminates; far above any real
/// node count for C1's two strategies.
const MAX_STEPS: usize = 10_000;
