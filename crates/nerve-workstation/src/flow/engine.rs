//! The pure step interpreter (design §3).
//!
//! [`step`] is a pure function `(FlowState, WorkflowDef, ledger) -> Vec<Action>`.
//! It reads the recorded results out of [`FlowState`] (which the driver fills
//! from the ledger, in declared order) and decides the next batch of
//! [`Action`]s. Every transition is a function of `(WorkflowDef, recorded
//! results)` — no wall-clock, no completion order, no RNG — so the orchestration
//! is golden-testable and replayable.

use super::{Action, FlowOutcome, NodeId, fold_results};
use crate::worker::TurnResult;
use nerve_runtime::{Strategy, WorkflowDef};
use std::collections::BTreeMap;

/// The interpreter's working state for one flow run. Carries only what `step`
/// needs: which nodes have been dispatched, and each finished node's recorded
/// [`TurnResult`]. Deterministic by construction — keyed by [`NodeId`], which is
/// a pure function of the strategy shape.
#[derive(Debug, Default)]
pub(crate) struct FlowState {
    /// Nodes the engine has already emitted a `StartWorker` for (so it does not
    /// re-dispatch them on the next `step`).
    dispatched: BTreeMap<NodeId, ()>,
    /// Finished nodes' results, keyed by node id. Populated by the driver after a
    /// worker turn completes (or fails).
    results: BTreeMap<NodeId, TurnResult>,
    /// Set once `step` has emitted `Terminate`, so the driver loop stops.
    terminated: bool,
}

impl FlowState {
    /// A fresh state for a not-yet-started flow.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Mark `node` as dispatched (the driver calls this when it applies a
    /// `StartWorker`, so the next `step` does not re-emit it).
    pub(crate) fn mark_dispatched(&mut self, node: NodeId) {
        self.dispatched.insert(node, ());
    }

    /// Record `node`'s finished result (the driver calls this after the worker
    /// turn resolves).
    pub(crate) fn record_result(&mut self, node: NodeId, result: TurnResult) {
        self.results.insert(node, result);
    }

    /// Whether the interpreter has terminated the flow.
    pub(crate) fn is_terminated(&self) -> bool {
        self.terminated
    }

    fn is_dispatched(&self, node: &NodeId) -> bool {
        self.dispatched.contains_key(node)
    }

    fn result(&self, node: &NodeId) -> Option<&TurnResult> {
        self.results.get(node)
    }
}

/// The pure interpreter (design §3). Given the current `state` and the
/// `def`inition, decide the next batch of [`Action`]s. C1 interprets
/// [`Strategy::Single`] and [`Strategy::Parallel`]; every other (defined-ahead)
/// variant terminates with an "unimplemented" outcome rather than panicking, so
/// the match is total and C5 only fills the arms.
///
/// The `ledger` argument is part of the interpreter signature (design §3) so a
/// later strategy can read the cross-worker blackboard from it; C1's two
/// strategies read only the recorded results carried in `state`.
pub(crate) fn step(state: &FlowState, def: &WorkflowDef) -> Vec<Action> {
    match &def.strategy {
        Strategy::Single { .. } => step_single(state),
        Strategy::Parallel { branches, join } => step_parallel(state, branches.len(), *join),
        // Defined-ahead strategies (design §3, C5): not yet interpreted. Terminate
        // deterministically with an explanatory outcome rather than diverging.
        other => vec![
            Action::Emit {
                outcome: unimplemented_outcome(strategy_name(other)),
            },
            Action::Terminate,
        ],
    }
}

/// `Single`: dispatch the one node, then on its recorded result emit + terminate.
fn step_single(state: &FlowState) -> Vec<Action> {
    let node = NodeId::single();
    match state.result(&node) {
        Some(result) => vec![
            Action::Emit {
                outcome: FlowOutcome {
                    ok: result.ok,
                    summary: format!(
                        "single: node ok={} ({} chars)",
                        result.ok,
                        result.text.len()
                    ),
                    results: vec![result.clone()],
                },
            },
            Action::Terminate,
        ],
        None if state.is_dispatched(&node) => Vec::new(), // running; wait
        None => vec![Action::StartWorker {
            node,
            step_index: 0,
        }],
    }
}

/// `Parallel`: dispatch ALL branches at once (one wave), then once every branch
/// has a recorded result, fold by `join` in **declared order** and terminate.
fn step_parallel(state: &FlowState, branch_count: usize, join: nerve_runtime::Join) -> Vec<Action> {
    if branch_count == 0 {
        return vec![
            Action::Emit {
                outcome: FlowOutcome {
                    ok: false,
                    results: Vec::new(),
                    summary: "parallel: no branches".to_string(),
                },
            },
            Action::Terminate,
        ];
    }
    let nodes: Vec<NodeId> = (0..branch_count).map(NodeId::branch).collect();
    // If any branch is undispatched, dispatch the whole wave at once.
    if nodes.iter().any(|node| !state.is_dispatched(node)) {
        return nodes
            .into_iter()
            .enumerate()
            .filter(|(_, node)| !state.is_dispatched(node))
            .map(|(step_index, node)| Action::StartWorker { node, step_index })
            .collect();
    }
    // All dispatched: wait until every branch has a recorded result.
    let mut results = Vec::with_capacity(branch_count);
    for node in &nodes {
        match state.result(node) {
            Some(result) => results.push(result.clone()),
            None => return Vec::new(), // still running; wait
        }
    }
    // Declared-order fold (results were collected in branch-index order).
    let outcome = fold_results(results, join);
    vec![Action::Emit { outcome }, Action::Terminate]
}

fn strategy_name(strategy: &Strategy) -> &'static str {
    match strategy {
        Strategy::Single { .. } => "single",
        Strategy::Parallel { .. } => "parallel",
        Strategy::Pipeline { .. } => "pipeline",
        Strategy::MapReduce { .. } => "map_reduce",
        Strategy::VoteJudge { .. } => "vote_judge",
        Strategy::Debate { .. } => "debate",
        Strategy::Hierarchical { .. } => "hierarchical",
        // `Strategy` is `#[non_exhaustive]`; a future variant reads as "unknown"
        // until its interpreter arm lands.
        _ => "unknown",
    }
}

fn unimplemented_outcome(name: &str) -> FlowOutcome {
    FlowOutcome {
        ok: false,
        results: Vec::new(),
        summary: format!("strategy `{name}` is not implemented in C1 (defined-ahead for C5)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::{Join, Step, TaskTemplate, WorkerRef};

    fn result(ok: bool, text: &str) -> TurnResult {
        TurnResult {
            ok,
            text: text.into(),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        }
    }

    fn cli_step(prompt: &str) -> Step {
        Step {
            worker: WorkerRef::Cli {
                name: "claude".into(),
            },
            task: TaskTemplate::new(prompt),
            autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
            on_fail: nerve_runtime::FailPolicy::Abort,
        }
    }

    fn single_def() -> WorkflowDef {
        WorkflowDef {
            schema_version: 1,
            name: "s".into(),
            strategy: Strategy::Single {
                step: cli_step("do it"),
            },
            budget: nerve_runtime::BudgetSpec::default(),
            max_depth: 2,
        }
    }

    fn parallel_def(n: usize, join: Join) -> WorkflowDef {
        WorkflowDef {
            schema_version: 1,
            name: "p".into(),
            strategy: Strategy::Parallel {
                branches: (0..n).map(|i| cli_step(&format!("task {i}"))).collect(),
                join,
            },
            budget: nerve_runtime::BudgetSpec::default(),
            max_depth: 2,
        }
    }

    #[test]
    fn single_dispatches_then_terminates_on_result() {
        let def = single_def();
        let mut state = FlowState::new();
        // First step: dispatch the one node.
        let actions = step(&state, &def);
        assert_eq!(
            actions,
            vec![Action::StartWorker {
                node: NodeId::single(),
                step_index: 0,
            }]
        );
        state.mark_dispatched(NodeId::single());
        // While running (no result yet), step does nothing.
        assert!(step(&state, &def).is_empty());
        // After the result lands, emit + terminate.
        state.record_result(NodeId::single(), result(true, "answer"));
        let actions = step(&state, &def);
        assert!(matches!(actions[0], Action::Emit { .. }));
        assert_eq!(actions[1], Action::Terminate);
    }

    #[test]
    fn parallel_dispatches_whole_wave_at_once() {
        let def = parallel_def(3, Join::All);
        let actions = step(&FlowState::new(), &def);
        let starts: Vec<usize> = actions
            .iter()
            .map(|a| match a {
                Action::StartWorker { step_index, .. } => *step_index,
                other => panic!("expected StartWorker, got {other:?}"),
            })
            .collect();
        assert_eq!(starts, vec![0, 1, 2], "all branches dispatched in order");
    }

    #[test]
    fn parallel_waits_for_all_branches_before_folding() {
        let def = parallel_def(2, Join::All);
        let mut state = FlowState::new();
        state.mark_dispatched(NodeId::branch(0));
        state.mark_dispatched(NodeId::branch(1));
        state.record_result(NodeId::branch(0), result(true, "a"));
        // Only one branch done: step waits.
        assert!(step(&state, &def).is_empty());
        state.record_result(NodeId::branch(1), result(true, "b"));
        let actions = step(&state, &def);
        assert!(matches!(actions[0], Action::Emit { .. }));
        assert_eq!(actions[1], Action::Terminate);
    }

    #[test]
    fn unimplemented_strategy_terminates_with_explanation() {
        let def = WorkflowDef {
            schema_version: 1,
            name: "pipe".into(),
            strategy: Strategy::Pipeline {
                stages: vec![cli_step("a")],
            },
            budget: nerve_runtime::BudgetSpec::default(),
            max_depth: 2,
        };
        let actions = step(&FlowState::new(), &def);
        match &actions[0] {
            Action::Emit { outcome } => {
                assert!(!outcome.ok);
                assert!(outcome.summary.contains("pipeline"));
            }
            other => panic!("expected Emit, got {other:?}"),
        }
        assert_eq!(actions[1], Action::Terminate);
    }
}
