//! `Strategy::Hierarchical` — a planner decides, then a bounded child flow runs.
//!
//! The phase machine (design §8):
//!
//! 1. **Planner** — run the single `planner` node (a `ProviderWorker` in practice). It
//!    decides whether work warrants a child flow.
//! 2. **Depth gate (absence-at-floor)** — the child flow runs at `depth + 1`. The
//!    engine spawns it ONLY while `depth + 1 < max_depth` (the `WorkflowDef.max_depth`
//!    ceiling, design §8). AT the ceiling the child is simply NOT spawned — a recorded
//!    `DepthCeiling` [`FlowDecision`], not a crash — and the planner's result is the
//!    flow's answer. This is the one bounded recursion model replacing the old
//!    `depth == 0` guard.
//! 3. **Child flow** — below the ceiling, run the `child` strategy as a NESTED flow by
//!    PROJECTING the parent state onto a `child/`-stripped sub-state, re-running the
//!    SAME [`step`](super::super::step) interpreter over the child strategy, then
//!    re-prefixing the returned actions ([`NodeId::nested`]). This reuses every C1–C5
//!    arm verbatim — the child's nodes record into the SHARED ledger under `child/…`,
//!    so replay stays byte-identical. A nested `Hierarchical` composes its own depth
//!    gate (its child is `child/child/…`), bounded by the same ceiling.
//!
//! Monotone de-escalation (design §6): a child node's autonomy/budget is intersected
//! with its parent's by the driver's `FleetBudget`/`node_grant`, so a child can only
//! narrow authority — the engine here only governs WHETHER a child runs (the depth
//! gate); the budget governs WITH WHAT.

use super::super::{FlowState, NodeId, child_def, step};
use super::emit_and_terminate;
use crate::flow::{Action, FlowOutcome};
use crate::worker::TurnResult;
use nerve_runtime::{FlowDecisionKind, Strategy, WorkflowDef};

/// Interpret one step of a `Hierarchical` (design §8). The dispatcher calls this at the
/// flow root (depth 0); a nested `Hierarchical` child recurses with `depth + 1`.
pub(in crate::flow::engine) fn step_hierarchical(
    state: &FlowState,
    def: &WorkflowDef,
    child: &Strategy,
) -> Vec<Action> {
    step_at_depth(state, def, child, 0)
}

/// Run a (possibly nested) `Hierarchical` whose planner is at `depth`. The depth is
/// threaded EXPLICITLY down the nesting (not recomputed from state), so the child-flow
/// nodes a level dispatches never inflate its own planner's depth.
fn step_at_depth(
    state: &FlowState,
    def: &WorkflowDef,
    child: &Strategy,
    depth: u32,
) -> Vec<Action> {
    let planner = NodeId::planner();
    // Phase 1: run the planner.
    let Some(planner_result) = state.result(&planner) else {
        return if state.is_dispatched(&planner) {
            Vec::new() // planner running; wait
        } else {
            vec![Action::StartWorker {
                node: planner,
                step_index: 0,
            }]
        };
    };
    // Phase 2: the depth gate. The child flow would run at `depth + 1`; spawn it only
    // below the ceiling (absence-at-floor).
    let child_depth = depth + 1;
    if child_depth >= def.max_depth {
        return refuse_child(planner_result.clone(), &planner, child_depth, def.max_depth);
    }
    // Phase 3: run the child strategy as a nested flow.
    run_child(state, def, child, depth, planner_result.clone())
}

/// Run the `child` strategy as a nested flow: project the parent state onto the
/// `child/`-stripped sub-state, interpret the child, then re-prefix the dispatched /
/// decision nodes and fold a child `Emit` into THIS flow's terminal outcome.
///
/// A nested `Hierarchical` child recurses through [`step_at_depth`] at `depth + 1` (so
/// the depth gate composes); every other child strategy is bounded + non-recursive, so
/// the shared [`step`] dispatcher interprets it over the sub-state directly.
fn run_child(
    state: &FlowState,
    def: &WorkflowDef,
    child: &Strategy,
    depth: u32,
    planner_result: TurnResult,
) -> Vec<Action> {
    let sub = state.project_child();
    let actions = if let Strategy::Hierarchical {
        child: grandchild, ..
    } = child
    {
        step_at_depth(&sub, def, grandchild, depth + 1)
    } else {
        step(&sub, &child_def(def, child.clone()))
    };
    actions
        .into_iter()
        .flat_map(|action| reproject(action, &planner_result))
        .collect()
}

/// Re-project one child-flow action into the parent namespace: prefix a `StartWorker`
/// / `Decision` node with `child/`, and fold a child `Emit` (+ its `Terminate`) into
/// THIS flow's terminal outcome (the hierarchy's answer is the child's, with the
/// planner kept as the lead result).
fn reproject(action: Action, planner_result: &TurnResult) -> Vec<Action> {
    match action {
        Action::StartWorker { node, step_index } => vec![Action::StartWorker {
            node: node.nested(),
            step_index,
        }],
        Action::Decision { node, kind } => vec![Action::Decision {
            node: node.nested(),
            kind,
        }],
        Action::Emit { outcome } => {
            emit_and_terminate(hierarchical_outcome(planner_result.clone(), outcome))
        }
        // The child's Terminate is folded into the Emit mapping above.
        Action::Terminate => Vec::new(),
        // The child interpreter emits none of these; pass through defensively.
        other => vec![other],
    }
}

/// The hierarchy's outcome when the child flow ran: ok iff the child flow was ok; the
/// planner result leads, then the child's kept results, so the audit shows both.
fn hierarchical_outcome(planner_result: TurnResult, child: FlowOutcome) -> FlowOutcome {
    let mut results = vec![planner_result];
    results.extend(child.results);
    FlowOutcome {
        ok: child.ok,
        summary: format!("hierarchical: planner + child ({})", child.summary),
        results,
    }
}

/// The depth-ceiling refusal (absence-at-floor, design §8): the planner ran but the
/// child flow is NOT spawned because `child_depth >= max_depth`. Records a
/// `DepthCeiling` decision and terminates with the planner's result as the answer.
fn refuse_child(
    planner_result: TurnResult,
    planner: &NodeId,
    child_depth: u32,
    max_depth: u32,
) -> Vec<Action> {
    let decision = Action::Decision {
        node: planner.clone(),
        kind: FlowDecisionKind::DepthCeiling {
            depth: child_depth,
            max_depth,
        },
    };
    let outcome = FlowOutcome {
        ok: planner_result.ok,
        summary: format!(
            "hierarchical: child refused at depth ceiling ({child_depth}/{max_depth}); planner only"
        ),
        results: vec![planner_result],
    };
    let mut actions = vec![decision];
    actions.extend(emit_and_terminate(outcome));
    actions
}
