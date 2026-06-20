//! `Strategy::MapReduce` — map a step over a deterministic context split, then reduce.
//!
//! The phase machine (design §3):
//!
//! 1. **Map** — the `over` [`ContextSplit`](nerve_runtime::ContextSplit) yields a
//!    fixed item count ([`split_len`]); the engine dispatches the `map` step once per
//!    item in ONE parallel wave (reuses the C1 fan-out + declared-order fold). Each
//!    map node's task interpolates its own split item (`{{split}}`) plus the shared
//!    context, resolved deterministically in the driver's `build_task`.
//! 2. **Reduce** — once every map node has a recorded result, dispatch the single
//!    `reduce` step (its task interpolates the map outputs from the ledger blackboard,
//!    in declared `map-0..map-n` order). Reduce is typically a `ProviderWorker`.
//! 3. **Done** — the reduce result is the map-reduce's answer.
//!
//! The split is a DATA decision (how many items), so the control flow stays a pure
//! function of the `WorkflowDef` — no live filesystem walk in the interpreter.

use super::super::{FlowOutcome, FlowState, NodeId};
use super::{collect_results, dispatch_wave, emit_and_terminate};
use crate::flow::Action;
use crate::worker::TurnResult;
use nerve_runtime::ContextSplit;

/// Interpret one step of a `MapReduce` (design §3). The `over` split determines how
/// many map workers run; they all run the same declared `map` step over their shard.
pub(in crate::flow::engine) fn step_map_reduce(
    state: &FlowState,
    over: &ContextSplit,
) -> Vec<Action> {
    let item_count = split_len(over);
    if item_count == 0 {
        return emit_and_terminate(FlowOutcome {
            ok: false,
            results: Vec::new(),
            summary: "map_reduce: empty context split".to_string(),
        });
    }
    let maps: Vec<NodeId> = (0..item_count).map(NodeId::map).collect();
    // Phase 1: dispatch the map wave (all at once) if any is undispatched.
    if let Some(actions) = dispatch_wave(state, &maps) {
        return actions;
    }
    // Wait for every map node (declared-order fold over the split).
    let Some(map_results) = collect_results(state, &maps) else {
        return Vec::new(); // maps still running; wait
    };
    let map_ok = map_results.iter().filter(|r| r.ok).count();
    // Phase 2 / 3: drive the reduce.
    let reduce = NodeId::reduce();
    match state.result(&reduce) {
        // Phase 3: reduce finished — its result is the answer.
        Some(reduce_result) => {
            emit_and_terminate(reduced_outcome(reduce_result.clone(), map_ok, item_count))
        }
        // Reduce running: wait.
        None if state.is_dispatched(&reduce) => Vec::new(),
        // Phase 2: dispatch the reduce over the (declared-order) map outputs.
        None => vec![Action::StartWorker {
            node: reduce,
            step_index: 0,
        }],
    }
}

/// The deterministic item count a [`ContextSplit`] yields (design §3): `Shards { n }`
/// → `n` map workers; `Paths { groups }` → one map worker per declared path group.
/// A pure function of the data — no filesystem walk — so the map-reduce control flow
/// stays golden-testable and replayable.
#[must_use]
pub(crate) fn split_len(over: &ContextSplit) -> usize {
    match over {
        ContextSplit::Shards { n } => *n as usize,
        ContextSplit::Paths { groups } => groups.len(),
    }
}

/// The deterministic split item handed to map worker `index` (design §3): the shard
/// number for `Shards`, or the joined path group for `Paths`. Interpolated into the
/// map task as `{{split}}` (the driver's `build_task`), so each map worker sees its
/// own shard — named-output substitution only, no expression language. `None` for an
/// out-of-range index (defensive; the engine only dispatches in-range nodes).
#[must_use]
pub(crate) fn split_item(over: &ContextSplit, index: usize) -> Option<String> {
    match over {
        ContextSplit::Shards { n } => (index < *n as usize).then(|| format!("shard {index}/{n}")),
        ContextSplit::Paths { groups } => groups.get(index).map(|paths| paths.join(", ")),
    }
}

/// The outcome of a finished map-reduce: the reduce result is the answer (kept), the
/// summary records how many map shards succeeded.
fn reduced_outcome(reduce_result: TurnResult, map_ok: usize, items: usize) -> FlowOutcome {
    FlowOutcome {
        ok: reduce_result.ok,
        summary: format!(
            "map_reduce: {map_ok}/{items} map shard(s) ok, reduce {}",
            if reduce_result.ok { "ok" } else { "failed" }
        ),
        results: vec![reduce_result],
    }
}
