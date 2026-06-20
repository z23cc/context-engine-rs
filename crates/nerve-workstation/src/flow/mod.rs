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

#[cfg(test)]
mod tests;

pub(crate) use driver::{Driver, FactoryResolver, FlowProgress, WorkerResolver};

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

    /// The id as a string slice (for ledger keys / logs).
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

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
