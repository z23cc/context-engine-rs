//! The driver — the only thing that applies [`Action`]s (design §3).
//!
//! The [`Driver`] runs the loop: call the pure [`engine::step`], apply the
//! returned [`Action`]s, then re-run `step` until [`Action::Terminate`]. Applying
//! a `StartWorker` mints a worker through the C0 [`WorkerFactory`], runs its turn,
//! and records every [`WorkerEvent`] + the final [`TurnResult`] into the
//! [`WorkerLedger`] — the only place nondeterminism enters. A `Parallel` wave is
//! dispatched through the **already-built** [`bounded_fan_out`], REUSED verbatim
//! (it preserves INPUT ORDER, the invariant behind the declared-order fold).

mod node;

use super::engine::{FlowState, step};
use super::resolve::{
    WorkerResolver, cancelled_outcome, is_steerable_strategy, refused_result,
    terminated_without_emit,
};
use super::{Action, FlowOutcome, NodeId};
use crate::subagent::bounded_fan_out;
use crate::worker::{
    BudgetDecision, BudgetLedger, BudgetSnapshot, FleetBudget, PathLeases, SpawnRefusal,
    SteerRegistry, TurnResult, WorkerEvent, WorkerLedger, WorkerSession,
};
use nerve_core::CancelToken;
use nerve_runtime::{WorkerRef, WorkflowDef};
use node::{NodeResult, NodeRun};
use std::sync::Arc;

/// One `StartWorker` instruction the driver dispatches: the node + its flat step
/// index into the strategy's step list.
type Dispatch = (NodeId, usize);

/// A budget partition of one wave (design §8): the spawns the [`FleetBudget`]
/// admits, and the ones it refuses at a ceiling (with the typed [`SpawnRefusal`]).
type BudgetPartition = (Vec<Dispatch>, Vec<(NodeId, SpawnRefusal)>);

/// A per-node `snapshot_generation` provider (design §5): given the def + node id,
/// it returns the generation to pin at that node's start. The host backs it with
/// the live snapshot (so a file mutation bumps a later node's generation); replay
/// backs it with the node's RECORDED generation, keeping the pin byte-identical.
pub(crate) type GenerationProvider = dyn Fn(&WorkflowDef, &str) -> u64 + Sync;

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
    /// The interpreter made a typed audit decision (design §4/§6): a vote tally, a
    /// judge pick, a debate round (the richer C5 strategies). The host maps it onto a
    /// [`RuntimeEvent::FlowDecision`](nerve_runtime::RuntimeEvent). Fired from the
    /// engine loop in the deterministic step order, so the audit trail is replayable.
    /// Default no-op for the hidden CLI / unbudgeted flows.
    fn decision(&self, _node: &str, _kind: &nerve_runtime::FlowDecisionKind) {}
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
    /// Optional per-node `snapshot_generation` provider (C4, design §5). The host
    /// backs it with the live snapshot so file mutations are pinned honestly per
    /// node-start; replay backs it with each node's recorded generation. Unset = `0`.
    generation: Option<&'a GenerationProvider>,
    /// Optional writer-node path-lease registry (C4, design §6): a deterministic
    /// engine-level lease that forbids two writer-nodes (Edit/Full autonomy) from
    /// running concurrently on overlapping path scope within one flow — both a safety
    /// property and the precondition for replay fidelity under mutation. Unset =
    /// no leasing (the C1/C2/C3 behaviour; a read-only flow needs none).
    leases: Option<&'a PathLeases>,
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
            generation: None,
            leases: None,
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

    /// Attach a per-node `snapshot_generation` provider (C4, design §5). The host
    /// backs it with the live snapshot (so a file mutation pins a later node's
    /// generation honestly); replay backs it with the recorded generation.
    #[must_use]
    pub(crate) fn with_generation(mut self, generation: &'a GenerationProvider) -> Self {
        self.generation = Some(generation);
        self
    }

    /// Attach a writer-node path-lease registry (C4, design §6): the engine then
    /// SERIALIZES writer-nodes (Edit/Full autonomy) that would race overlapping path
    /// scope within a wave — a deterministic safety property + replay-fidelity
    /// precondition. A read-only flow needs none (every node is a reader).
    #[must_use]
    pub(crate) fn with_leases(mut self, leases: &'a PathLeases) -> Self {
        self.leases = Some(leases);
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
                    Action::Decision { node, kind } => {
                        // A pure audit decision (vote tally / judge pick / debate
                        // round): fire the observer in step order. Recorded nowhere on
                        // the tape (the events it summarizes already are), so replay
                        // reproduces it deterministically from the same results.
                        if let Some(observer) = self.observer {
                            observer.decision(node.as_str(), &kind);
                        }
                    }
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
                start,
            } = run;
            // Record the node-start FIRST (the rendered prompt + pinned snapshot
            // generation, design §5) so the tape is a self-contained replay source,
            // then the buffered events, then the result — all in declared order.
            if let Some((prompt, generation)) = start {
                self.ledger.record_start(node.as_str(), &prompt, generation);
            }
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
}
/// Safety net against an interpreter that never terminates; far above any real
/// node count for C1's two strategies.
const MAX_STEPS: usize = 10_000;
