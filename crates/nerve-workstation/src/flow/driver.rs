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
use super::{Action, FlowOutcome, NodeId};
use crate::subagent::bounded_fan_out;
use crate::worker::{
    AgentWorker, BudgetGrant, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerFactory,
    WorkerKind, WorkerLedger, WorkerTask,
};
use nerve_core::CancelToken;
use nerve_runtime::{Step, Strategy, WorkerRef, WorkflowDef};
use std::sync::Arc;

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
}

/// How the driver resolves a [`WorkerRef`] into a runnable [`AgentWorker`]. The
/// production driver uses the C0 [`WorkerFactory`]; tests inject a closure that
/// hands back scripted `FakeWorker`s / `ReplayWorker`s, so the loop is testable
/// without a real LLM/subprocess.
pub(crate) trait WorkerResolver: Sync {
    /// Mint the worker for `worker_ref` (or fail before any spawn).
    fn resolve(&self, worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError>;
}

/// The production resolver: maps a [`WorkerRef`] onto a [`WorkerKind`] and mints
/// it through the C0 [`WorkerFactory`].
pub(crate) struct FactoryResolver {
    factory: WorkerFactory,
}

impl FactoryResolver {
    pub(crate) fn new(factory: WorkerFactory) -> Self {
        Self { factory }
    }
}

impl WorkerResolver for FactoryResolver {
    fn resolve(&self, worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let kind = worker_kind(worker_ref)?;
        self.factory.create(kind)
    }
}

/// Translate a declarative [`WorkerRef`] into the C0 [`WorkerKind`] the factory
/// mints. `Named` is defined-ahead (design §6, C6) and refused here — C1 does not
/// resolve named worker defs yet.
fn worker_kind(worker_ref: &WorkerRef) -> Result<WorkerKind, WorkerError> {
    match worker_ref {
        WorkerRef::Cli { name } => {
            // The factory's `Cli` kind takes a `&'static str` catalog name; map
            // the data string onto the known catalog (an unknown name is refused
            // before any spawn, mirroring the factory's own check).
            let catalog = match name.as_str() {
                "codex" => "codex",
                "claude" => "claude",
                "gemini" => "gemini",
                other => {
                    return Err(WorkerError::Start(format!(
                        "unknown CLI worker `{other}` (expected codex|claude|gemini)"
                    )));
                }
            };
            Ok(WorkerKind::Cli(catalog))
        }
        WorkerRef::Provider { provider, model } => Ok(WorkerKind::Provider {
            provider: provider.clone(),
            model: model.clone(),
        }),
        WorkerRef::Named { name } => Err(WorkerError::Start(format!(
            "named worker `{name}` is not resolvable in C1 (WorkerDef loading lands in C6)"
        ))),
    }
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

    /// Run `def` to completion: loop `step` → apply actions → record results,
    /// until `Terminate`. Returns the emitted [`FlowOutcome`] (or a terminal
    /// outcome if the flow never emitted one — e.g. it was cancelled).
    pub(crate) fn run(&self, def: &WorkflowDef, cancel: &CancelToken) -> FlowOutcome {
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
                    // C1's interpreter never emits these (declared-ahead, C3+).
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
        let inputs: Vec<(NodeId, usize)> = starts.to_vec();
        let results = bounded_fan_out(
            inputs,
            self.concurrency,
            cancel,
            |(node, step_index)| {
                let (result, events) = self.run_node(def, &node, step_index, cancel);
                NodeResult {
                    node,
                    result,
                    events,
                }
            },
            |(node, _)| NodeResult {
                node: node.clone(),
                result: failed_result("cancelled"),
                events: Vec::new(),
            },
            || NodeResult {
                // A worker thread that panicked has no node id to recover; the
                // node was still marked dispatched, so it folds as a failure.
                node: NodeId::single(),
                result: failed_result("worker thread panicked"),
                events: Vec::new(),
            },
        );
        // bounded_fan_out preserves INPUT ORDER, so `results` is in declared
        // branch order. Writing the ledger here (in declared order) — NOT inside
        // the concurrent worker closures — is what makes the replay tape a
        // deterministic function of declared order, regardless of which branch
        // finished first (design §3, the determinism invariant that the
        // byte-identical replay gate enforces).
        for node_result in results {
            let NodeResult {
                node,
                result,
                events,
            } = node_result;
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
            state.record_result(node, result);
        }
    }

    /// Run one node's worker turn end-to-end: resolve the worker, render its task
    /// from the ledger blackboard, start it (turn 1), and return its
    /// [`TurnResult`] together with the events it emitted (BUFFERED, not written
    /// to the ledger here — the caller writes them in declared order so the tape
    /// is deterministic). The live progress sink still fires during the turn for
    /// real-time display. A start/turn error maps to a failed result so a sibling
    /// branch is never aborted (mirrors `bounded_fan_out`'s per-task isolation).
    fn run_node(
        &self,
        def: &WorkflowDef,
        node: &NodeId,
        step_index: usize,
        cancel: &CancelToken,
    ) -> (TurnResult, Vec<WorkerEvent>) {
        let Some(step_def) = step_for(def, step_index) else {
            return (
                failed_result(&format!("no step at index {step_index}")),
                Vec::new(),
            );
        };
        let worker = match self.resolver.resolve(&step_def.worker) {
            Ok(worker) => worker,
            Err(err) => return (failed_result(&format!("resolve failed: {err}")), Vec::new()),
        };
        // A node whose worker resolved is genuinely starting: fire the lifecycle
        // observer (C2 maps this to FlowNodeStarted) with the declared WorkerRef.
        if let Some(observer) = self.observer {
            observer.node_started(node.as_str(), &step_def.worker);
        }
        let task = self.build_task(step_def);
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
            Ok(mut session) => {
                let result = session.result();
                session.close();
                (result, events)
            }
            Err(WorkerError::Cancelled) => (failed_result("cancelled"), events),
            Err(err) => (failed_result(&err.to_string()), events),
        }
    }

    /// Render a step's task template, interpolating `{{node}}` placeholders from
    /// the ledger blackboard (upstream outputs), then build the [`WorkerTask`].
    fn build_task(&self, step_def: &Step) -> WorkerTask {
        let ledger = Arc::clone(&self.ledger);
        let prompt = step_def.task.render(&|name| ledger.output(name));
        WorkerTask {
            prompt,
            autonomy: step_def.autonomy,
            model: provider_model(&step_def.worker),
            tool_filter: None,
            // C1 threads an empty grant (recorded, not enforced — design §6, C3).
            budget: BudgetGrant::default(),
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
    result: TurnResult,
    events: Vec<WorkerEvent>,
}

/// Look up the declared [`Step`] for a flat `step_index` across the C1 strategies.
fn step_for(def: &WorkflowDef, step_index: usize) -> Option<&Step> {
    match &def.strategy {
        Strategy::Single { step } if step_index == 0 => Some(step),
        Strategy::Parallel { branches, .. } => branches.get(step_index),
        _ => None,
    }
}

/// A provider worker's model override comes from the `WorkerRef`; a CLI worker
/// takes its model from config, so `None` here.
fn provider_model(worker_ref: &WorkerRef) -> Option<String> {
    match worker_ref {
        WorkerRef::Provider { model, .. } => Some(model.clone()),
        WorkerRef::Cli { .. } | WorkerRef::Named { .. } => None,
    }
}

fn failed_result(reason: &str) -> TurnResult {
    TurnResult {
        ok: false,
        text: format!("worker failed: {reason}"),
        usage: nerve_agent::Usage::default(),
        cost_usd: None,
        timed_out: false,
    }
}

fn cancelled_outcome() -> FlowOutcome {
    FlowOutcome {
        ok: false,
        results: Vec::new(),
        summary: "flow cancelled".to_string(),
    }
}

fn terminated_without_emit() -> FlowOutcome {
    FlowOutcome {
        ok: false,
        results: Vec::new(),
        summary: "flow terminated without emitting an outcome".to_string(),
    }
}

/// Safety net against an interpreter that never terminates; far above any real
/// node count for C1's two strategies.
const MAX_STEPS: usize = 10_000;
