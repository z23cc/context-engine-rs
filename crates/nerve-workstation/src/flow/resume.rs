//! Resume = replay-then-continue (Wave C4, design §5) — the node-boundary contract.
//!
//! North-star §5 keeps live jobs in-memory, so a daemon restart loses the live
//! worker threads. The design's resolution is **persist the ledger, not the threads**
//! (the [`FlowStore`](crate::flow_store)) and resume by REPLAYING the recorded tape
//! through the SAME deterministic interpreter to rebuild scheduler + blackboard state
//! to the LAST recorded node boundary, then scheduling the PENDING nodes live.
//!
//! This module ships the load-bearing, deterministic half of that contract:
//!
//! - [`replay_to_boundary`] folds a recorded [`WorkerLedger`] back into a
//!   [`WorkerLedger`] via the engine's REPLAY path (so the blackboard + the recorded
//!   results are rebuilt exactly), and
//! - [`pending_nodes`] computes — as a PURE function of the [`WorkflowDef`] + the
//!   finished-node set — which nodes still need to run.
//!
//! ## Live-continue is a documented follow-on
//!
//! Actually re-dispatching the pending nodes against live workers (so a restarted
//! daemon finishes an interrupted flow) is deferred: it needs the in-process worker
//! `ResumeState` seam wired through the engine and a mid-flight CLI node re-dispatched
//! from its last recorded instruction (never silently resumed — a CLI child cannot
//! survive process death). The replay-to-boundary + pending computation here is the
//! deterministic precondition for it, and is what the byte-identical gate guarantees
//! is faithful. See design §5 "Resume = replay, then continue" + open question 1.
#![allow(
    dead_code,
    reason = "C4 ships the deterministic replay-to-boundary + pending-nodes half; the live \
              re-dispatch caller (flow resume) is the documented follow-on (design §5)"
)]

use super::{Driver, ReplayResolver, replay_generation_provider};
use crate::worker::{WorkerLedger, WorkerSession};
use nerve_core::CancelToken;
use nerve_runtime::{Strategy, WorkflowDef};
use std::sync::Arc;

/// Replay a recorded ledger through the engine's REPLAY path to rebuild the
/// blackboard + recorded-result state to the last recorded node boundary (design §5).
/// Returns the freshly-rebuilt [`WorkerLedger`] (byte-identical to the recorded one
/// for the nodes that finished), ready to seed a live continuation. Deterministic:
/// the replay re-emits only the recorded turns, touching no LLM/subprocess.
#[must_use]
pub(crate) fn replay_to_boundary(def: &WorkflowDef, recorded: &WorkerLedger) -> Arc<WorkerLedger> {
    let resolver = ReplayResolver::from_ledger(recorded);
    let generation = replay_generation_provider(recorded);
    let rebuilt = Arc::new(WorkerLedger::new());
    // A deny-all approver + no budget/leases: replay re-emits recorded events only.
    let approver: Arc<dyn crate::delegate_proxy::DelegateApprover> = Arc::new(ResumeApprover);
    Driver::new(&resolver, Arc::clone(&rebuilt), approver, None)
        .with_generation(&generation)
        .run(def, &CancelToken::never());
    rebuilt
}

/// The nodes that still need to run to finish `def`, given the set of nodes that
/// already have a recorded `Result` (the last node boundary). A PURE function of the
/// strategy shape + the finished set — the deterministic "what's left" computation a
/// live continuation would schedule.
///
/// - `Single`: `node-0` is pending unless finished.
/// - `Parallel`: every `branch-i` not yet finished (a wave; order does not matter).
/// - `Pipeline`: the FIRST not-yet-finished `stage-i` in declared order, plus every
///   later stage — but a pipeline runs them strictly sequentially, so the immediate
///   frontier is the first pending stage (the rest follow once it lands).
/// - Other (defined-ahead) strategies: empty (their resume lands with their wave).
#[must_use]
pub(crate) fn pending_nodes(def: &WorkflowDef, finished: &[String]) -> Vec<String> {
    let is_finished = |node: &str| finished.iter().any(|f| f == node);
    match &def.strategy {
        Strategy::Single { .. } => single_pending(&is_finished),
        Strategy::Parallel { branches, .. } => parallel_pending(branches.len(), &is_finished),
        Strategy::Pipeline { stages } => pipeline_pending(stages.len(), &is_finished),
        _ => Vec::new(),
    }
}

fn single_pending(is_finished: &dyn Fn(&str) -> bool) -> Vec<String> {
    if is_finished("node-0") {
        Vec::new()
    } else {
        vec!["node-0".to_string()]
    }
}

fn parallel_pending(branches: usize, is_finished: &dyn Fn(&str) -> bool) -> Vec<String> {
    (0..branches)
        .map(|i| format!("branch-{i}"))
        .filter(|node| !is_finished(node))
        .collect()
}

fn pipeline_pending(stages: usize, is_finished: &dyn Fn(&str) -> bool) -> Vec<String> {
    (0..stages)
        .map(|i| format!("stage-{i}"))
        .filter(|node| !is_finished(node))
        .collect()
}

/// The approver a resume replay runs under: a replay re-emits recorded events and
/// never raises a live approval, so this is a deny-all sentinel that is never consulted.
struct ResumeApprover;

impl crate::delegate_proxy::DelegateApprover for ResumeApprover {
    fn request(
        &self,
        _session_id: &str,
        _tool: &str,
        _args: &serde_json::Value,
        _tier: nerve_runtime::RiskTier,
        _preview: String,
        _cancel: &CancelToken,
    ) -> nerve_runtime::SessionApprovalDecision {
        nerve_runtime::SessionApprovalDecision::Deny
    }
}

// A trivially-unused marker so the WorkerSession import documents the resume seam
// (a live continuation re-dispatches pending nodes, producing fresh `WorkerSession`s).
#[allow(dead_code)]
type ResumeSession = Box<dyn WorkerSession>;

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::{
        BudgetSpec, DelegateAutonomy, FailPolicy, Join, Step, TaskTemplate, WorkerRef,
    };

    fn step(prompt: &str) -> Step {
        Step {
            worker: WorkerRef::Cli {
                name: "claude".into(),
            },
            task: TaskTemplate::new(prompt),
            autonomy: DelegateAutonomy::ReadOnly,
            on_fail: FailPolicy::Continue,
        }
    }

    fn def(strategy: Strategy) -> WorkflowDef {
        WorkflowDef {
            schema_version: 1,
            name: "resume".into(),
            strategy,
            budget: BudgetSpec::default(),
            max_depth: 2,
        }
    }

    #[test]
    fn pending_nodes_for_single() {
        let d = def(Strategy::Single { step: step("only") });
        assert_eq!(pending_nodes(&d, &[]), vec!["node-0".to_string()]);
        assert_eq!(
            pending_nodes(&d, &["node-0".to_string()]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn pending_nodes_for_parallel_excludes_finished_branches() {
        let d = def(Strategy::Parallel {
            branches: vec![step("a"), step("b"), step("c")],
            join: Join::All,
        });
        // Branch 1 already finished → branches 0 and 2 remain (declared order).
        let pending = pending_nodes(&d, &["branch-1".to_string()]);
        assert_eq!(
            pending,
            vec!["branch-0".to_string(), "branch-2".to_string()]
        );
        // All finished → none pending.
        assert!(
            pending_nodes(
                &d,
                &["branch-0".into(), "branch-1".into(), "branch-2".into()]
            )
            .is_empty()
        );
    }

    #[test]
    fn pending_nodes_for_pipeline_is_the_unfinished_tail() {
        let d = def(Strategy::Pipeline {
            stages: vec![step("s0"), step("s1"), step("s2")],
        });
        // Stages 0 and 1 finished → stage 2 is the frontier.
        assert_eq!(
            pending_nodes(&d, &["stage-0".into(), "stage-1".into()]),
            vec!["stage-2".to_string()]
        );
        // Nothing finished → all pending, in declared order.
        assert_eq!(
            pending_nodes(&d, &[]),
            vec![
                "stage-0".to_string(),
                "stage-1".to_string(),
                "stage-2".to_string()
            ]
        );
    }

    #[test]
    fn replay_to_boundary_rebuilds_the_blackboard_for_finished_nodes() {
        // RECORD a 3-stage pipeline, then replay-to-boundary rebuilds the same
        // blackboard (the resume seam: the rebuilt ledger seeds a live continuation).
        let d = def(Strategy::Pipeline {
            stages: vec![step("alpha"), step("beta"), step("gamma")],
        });
        let recorded = crate::flow::tests::record_for_resume(&d);
        // All three stages finished in the recorded run.
        assert_eq!(
            recorded.finished_nodes(),
            vec![
                "stage-0".to_string(),
                "stage-1".to_string(),
                "stage-2".to_string()
            ]
        );
        let rebuilt = replay_to_boundary(&d, &recorded);
        // The rebuilt tape is byte-identical (replay fidelity) and the blackboard is
        // restored, so a continuation reads the same upstream outputs.
        assert_eq!(rebuilt.to_jsonl(), recorded.to_jsonl());
        assert_eq!(rebuilt.output("stage-0"), recorded.output("stage-0"));
        // And `pending_nodes` over the rebuilt finished-set is empty (fully recorded).
        assert!(pending_nodes(&d, &rebuilt.finished_nodes()).is_empty());
    }
}
