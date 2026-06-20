//! The deterministic orchestration engine (Wave C1).
//!
//! This is the conductor the orchestration design
//! (`docs/designs/agent-orchestration.md` §3) calls for: a **pure interpreter**
//! over a recorded worker tape. The control flow is a deterministic function of
//! `(WorkflowDef, recorded WorkerResults)`; the only nondeterminism — each
//! worker's events, usage, cost, and timing — is captured into the
//! [`WorkerLedger`](crate::worker::WorkerLedger) (§5). So an orchestration run is
//! reproducible regardless of which worker finishes first.
//!
//! ## The pieces
//!
//! - [`engine::step`] — the pure interpreter: `(FlowState, WorkflowDef, ledger)`
//!   → `Vec<Action>`. C1 interprets `Strategy::Single` + `Strategy::Parallel`.
//! - [`driver::Driver`] — applies [`Action`]s by minting workers through the C0
//!   [`WorkerFactory`](crate::worker::WorkerFactory), recording every
//!   [`WorkerEvent`](crate::worker::WorkerEvent) / [`TurnResult`] into the ledger,
//!   then re-running `step` until [`Action::Terminate`].
//! - The fan-out primitive is the **already-built**
//!   [`bounded_fan_out`](crate::subagent) — REUSED verbatim, preserving INPUT
//!   ORDER, which is the determinism invariant behind the declared-order fold.
//!
//! ## ZERO protocol commitment (C1)
//!
//! The engine is driven only by a hidden `nerve flow run` CLI subcommand
//! (`commands::flow`). It adds NO `RuntimeCommand`/`RuntimeEvent` vocabulary; C2
//! lands the `flow.*` protocol on top of this hardened engine.
#![allow(
    dead_code,
    unused_imports,
    reason = "C1 engine surface; the hidden `flow run` CLI + C2 protocol + tests are its callers"
)]

mod driver;
mod engine;
mod resolve;
mod resume;
mod safety;

#[cfg(test)]
mod tests;

pub(crate) use driver::{Driver, FlowObserver, FlowProgress};
pub(crate) use engine::{split_item, split_len};
pub(crate) use resolve::{
    FactoryResolver, ReplayResolver, WorkerResolver, replay_generation_provider,
};
pub(crate) use safety::{WorkflowError, validate_workflow};

use crate::worker::TurnResult;
use nerve_runtime::Join;

/// A stable identifier for one worker node in a flow run. Deterministic: it is a
/// pure function of the strategy shape (the branch index), never of completion
/// order or wall-clock — the ledger and any future protocol event key off it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(String);

impl NodeId {
    /// The node id for a `Single` strategy's one step.
    fn single() -> Self {
        Self("node-0".to_string())
    }

    /// The node id for the `index`-th branch of a `Parallel` strategy.
    fn branch(index: usize) -> Self {
        Self(format!("branch-{index}"))
    }

    /// The node id for the `index`-th stage of a `Pipeline` strategy. A downstream
    /// stage interpolates an upstream stage's output from the ledger blackboard by
    /// this id (e.g. `{{stage-0}}`), and `flow.steer`'s [`WorkerSelector`] targets a
    /// live stage by it.
    fn stage(index: usize) -> Self {
        Self(format!("stage-{index}"))
    }

    /// The node id for the `index`-th candidate of a `VoteJudge` strategy (design §3).
    /// The judge interpolates each candidate's output by this id (e.g. `{{cand-0}}`).
    fn candidate(index: usize) -> Self {
        Self(format!("cand-{index}"))
    }

    /// The node id for a `VoteJudge`/`Debate` strategy's adjudicating judge.
    fn judge() -> Self {
        Self("judge".to_string())
    }

    /// The node id for the `index`-th map worker of a `MapReduce` strategy (one per
    /// `ContextSplit` item). The reduce interpolates each by this id (e.g. `{{map-0}}`).
    fn map(index: usize) -> Self {
        Self(format!("map-{index}"))
    }

    /// The node id for a `MapReduce` strategy's reduce worker.
    fn reduce() -> Self {
        Self("reduce".to_string())
    }

    /// The node id for `side` `s` in `round` `r` of a `Debate` strategy (design §3).
    /// A later round interpolates an earlier round's argument by this id
    /// (e.g. `{{side-0-round-0}}`).
    fn debate_turn(side: usize, round: usize) -> Self {
        Self(format!("side-{side}-round-{round}"))
    }

    /// The node id for a `Hierarchical` strategy's planner (design §8). It runs first
    /// and decides whether a child flow may spawn.
    fn planner() -> Self {
        Self("planner".to_string())
    }

    /// Prefix this node id so it is unique inside a child flow (design §8): a
    /// `Hierarchical` strategy runs its child as a nested flow, recording into the
    /// SAME ledger, so the child's `node-0` becomes `child/node-0`. Nesting composes
    /// (`child/child/node-0`), bounded by the depth ceiling.
    fn nested(&self) -> Self {
        Self(format!("{CHILD_PREFIX}{}", self.0))
    }

    /// Strip ONE `child/` prefix, projecting a parent node id into the child-flow
    /// namespace ([`Self::nested`]'s inverse), or `None` for a non-child node. The
    /// `Hierarchical` arm uses this to project the parent state onto the child's view.
    fn strip_nested(&self) -> Option<Self> {
        self.0
            .strip_prefix(CHILD_PREFIX)
            .map(|inner| Self(inner.to_string()))
    }

    /// The id as a string slice (for ledger keys / logs).
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// The path prefix the engine prepends to a child flow's node ids (design §8), so a
/// `Hierarchical` child's nodes never collide with the parent's in the shared ledger.
pub(crate) const CHILD_PREFIX: &str = "child/";

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One scheduling instruction the pure [`engine::step`] interpreter emits; the
/// [`driver::Driver`] is the only thing that applies them (design §3). C1 emits
/// `StartWorker`, `Emit`, and `Terminate`; the steer/close/approval actions are
/// part of the declared vocabulary (the design's `Action` set) and land with the
/// richer strategies (C3+), so the interpreter is total over the full set.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    /// Start the worker for `node` on the rendered task. The driver mints it via
    /// the [`WorkerFactory`](crate::worker::WorkerFactory) and records its events.
    StartWorker {
        node: NodeId,
        /// Index into the strategy's step list (`0` for `Single`, the branch
        /// index for `Parallel`) so the driver can fetch the declared `Step`.
        step_index: usize,
    },
    /// Inject a follow-up into a live worker node. Declared-ahead for C3 (steer);
    /// C1's interpreter never emits it.
    SteerWorker { node: NodeId, message: String },
    /// Tear a worker node down. Declared-ahead for C3; C1 closes nodes in the
    /// driver's own teardown, so the interpreter never emits this.
    CloseWorker { node: NodeId },
    /// Request operator approval. Declared-ahead for the protocol wave (C2/C4);
    /// C1 routes CLI approvals through the existing hub, not this action.
    RequestApproval { node: NodeId, request_id: String },
    /// Record a typed audit decision the interpreter made (design §4/§6): a vote
    /// tally, a judge pick, a debate round. Pure — a deterministic function of the
    /// recorded results — so it replays byte-identically. The driver fires the
    /// [`FlowObserver::decision`] callback (which the host maps onto a
    /// [`RuntimeEvent::FlowDecision`](nerve_runtime::RuntimeEvent)); it records NO
    /// ledger entry (the events/results the decision summarizes are already on the
    /// tape), so replay reproduces it from the same recorded results.
    Decision {
        node: NodeId,
        kind: nerve_runtime::FlowDecisionKind,
    },
    /// Emit the flow's aggregated outcome (the fold of recorded results).
    Emit { outcome: FlowOutcome },
    /// The flow is finished — stop the driver loop.
    Terminate,
}

/// The aggregated result of a finished flow: the folded [`TurnResult`]s in
/// **declared step order** (never completion order — the load-bearing invariant,
/// design §3) plus whether the flow as a whole succeeded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FlowOutcome {
    /// Whether the flow succeeded under its join/fail policy.
    pub(crate) ok: bool,
    /// The kept results, in declared step order.
    pub(crate) results: Vec<TurnResult>,
    /// A one-line human-readable summary of how the flow terminated.
    pub(crate) summary: String,
}

impl FlowOutcome {
    /// The concatenated text of the kept results (the flow's "answer"), joined by
    /// a blank line — what the hidden CLI prints and a future `flow.completed`
    /// event would carry.
    pub(crate) fn final_text(&self) -> String {
        self.results
            .iter()
            .map(|r| r.text.as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Fold `results` (already in declared step order) by `join`. THE load-bearing
/// invariant (design §3): the fold is over declared order, so the outcome is
/// independent of which branch finished first. Pure and total over [`Join`].
///
/// - [`Join::All`] keeps every result; ok iff every kept result is ok.
/// - [`Join::FirstOk`] keeps the first ok result in declared order; not ok if
///   none succeeded (it then keeps all results so the failure is inspectable).
/// - [`Join::Quorum`] keeps the first `n` ok results in declared order; ok iff at
///   least `n` succeeded. A short quorum keeps whatever oks there were (not ok).
fn fold_results(results: Vec<TurnResult>, join: Join) -> FlowOutcome {
    match join {
        Join::All => {
            let ok = results.iter().all(|r| r.ok);
            let summary = format!(
                "join=all: {}/{} branches ok",
                results.iter().filter(|r| r.ok).count(),
                results.len()
            );
            FlowOutcome {
                ok,
                results,
                summary,
            }
        }
        Join::FirstOk => match results.iter().position(|r| r.ok) {
            Some(index) => {
                let kept = results[index].clone();
                FlowOutcome {
                    ok: true,
                    results: vec![kept],
                    summary: format!("join=first_ok: branch {index} (declared order)"),
                }
            }
            None => FlowOutcome {
                ok: false,
                summary: format!("join=first_ok: no branch ok ({} attempted)", results.len()),
                results,
            },
        },
        Join::Quorum { n } => {
            let oks: Vec<TurnResult> = results.into_iter().filter(|r| r.ok).collect();
            let needed = n as usize;
            let reached = oks.len() >= needed;
            let kept: Vec<TurnResult> = oks.into_iter().take(needed.max(1)).collect();
            FlowOutcome {
                ok: reached,
                summary: format!(
                    "join=quorum(n={n}): {} ok, {}",
                    kept.len(),
                    if reached { "reached" } else { "short" }
                ),
                results: kept,
            }
        }
    }
}
