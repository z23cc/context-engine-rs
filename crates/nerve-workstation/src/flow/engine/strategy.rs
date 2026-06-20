//! The richer C5 strategy interpreters (design §3, Wave C5).
//!
//! Each arm here is a **pure phase machine**: a deterministic function of the
//! current [`FlowState`](super::FlowState) (the recorded results, in declared order)
//! that decides the next batch of [`Action`](crate::flow::Action)s. The four
//! strategies — [`step_vote_judge`], [`step_map_reduce`], [`step_debate`],
//! [`step_hierarchical`] — share the same discipline as C1's `Single`/`Parallel`:
//!
//! - **declared-order fold, never completion order** (the load-bearing invariant) —
//!   every phase reads results back in declared `NodeId` order;
//! - **once-only decisions** — an audit [`Action::Decision`](crate::flow::Action) is
//!   co-emitted with the `StartWorker` that opens the *next* phase, so it fires
//!   exactly once (the next `step` finds that node dispatched and waits), keeping the
//!   audit trail replayable;
//! - **no nondeterminism** — no wall-clock, no RNG, no live ledger reads in the
//!   control flow (output interpolation is the driver's `build_task`, off the tape).
//!
//! Split out of `engine.rs` per the file-size convention (one file per strategy
//! family); the dispatcher in [`super::step`] routes each `Strategy` variant here.

mod debate;
mod hierarchical;
mod mapreduce;
mod vote;

pub(super) use debate::step_debate;
pub(super) use hierarchical::step_hierarchical;
pub(super) use mapreduce::step_map_reduce;
pub(super) use vote::step_vote_judge;

// The deterministic context-split sizing/items — used by the driver's `build_task`
// (to interpolate each map shard's `{{split}}`) and the flow_job DAG-edge emitter.
pub(crate) use mapreduce::{split_item, split_len};

use super::{FlowState, NodeId};
use crate::flow::Action;
use crate::worker::TurnResult;

/// Dispatch a parallel WAVE of `nodes` (every undispatched one at once), or `None`
/// if the whole wave is already dispatched. The shared fan-out primitive every
/// multi-worker phase (vote candidates, map items, a debate round) reuses, so they
/// all inherit the C1 declared-order fold + `bounded_fan_out` semantics. The
/// `step_index` carried on each `StartWorker` is the node's index within its phase
/// (the driver resolves the actual `Step` by node id).
fn dispatch_wave(state: &FlowState, nodes: &[NodeId]) -> Option<Vec<Action>> {
    if nodes.iter().all(|node| state.is_dispatched(node)) {
        return None;
    }
    Some(
        nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| !state.is_dispatched(node))
            .map(|(step_index, node)| Action::StartWorker {
                node: node.clone(),
                step_index,
            })
            .collect(),
    )
}

/// Collect the recorded results for `nodes` in declared order, or `None` if any node
/// has not finished yet (the phase is still running — the caller waits). The
/// declared-order fold lives here: results come back keyed by `nodes` order, never by
/// which worker finished first.
fn collect_results(state: &FlowState, nodes: &[NodeId]) -> Option<Vec<TurnResult>> {
    let mut results = Vec::with_capacity(nodes.len());
    for node in nodes {
        results.push(state.result(node)?.clone());
    }
    Some(results)
}

/// The terminal `[Emit, Terminate]` pair an interpreter returns once a phase machine
/// reaches its final outcome.
fn emit_and_terminate(outcome: crate::flow::FlowOutcome) -> Vec<Action> {
    vec![Action::Emit { outcome }, Action::Terminate]
}
