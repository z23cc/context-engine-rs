//! `Strategy::VoteJudge` — generate candidates, require a quorum, let a judge pick.
//!
//! The phase machine (design §3):
//!
//! 1. **Candidates** — dispatch all `candidates` in ONE parallel wave (reuses the C1
//!    fan-out + declared-order fold).
//! 2. **Tally** — once every candidate has a recorded result, count the successes.
//!    `k` is the quorum: how many candidate successes are required before the judge
//!    runs. Emit a `VoteTally` audit decision. If fewer than `k` succeeded the vote is
//!    SHORT — short-circuit to a not-ok outcome WITHOUT running the judge (there is
//!    nothing viable to adjudicate).
//! 3. **Judge** — with quorum reached, dispatch the single `judge` node (its task
//!    interpolates the candidate outputs from the ledger blackboard, in the driver).
//!    The `VoteTally` is co-emitted with the judge's `StartWorker`, so it fires
//!    exactly once.
//! 4. **Pick** — once the judge finishes, emit a `JudgePick` decision and the final
//!    outcome (the judge's result is the vote's answer; the kept candidates trail it).
//!
//! Mixed-substrate (design §7): candidates are commonly CLI workers, the judge a
//! `ProviderWorker` — the engine sees only `AgentWorker`, so it does not care.

use super::super::{FlowOutcome, FlowState, NodeId};
use super::{collect_results, dispatch_wave, emit_and_terminate};
use crate::flow::Action;
use crate::worker::TurnResult;
use nerve_runtime::FlowDecisionKind;

/// Interpret one step of a `VoteJudge` (design §3). `candidate_count` candidates run
/// in parallel; `k` successes are the quorum required before the judge adjudicates.
pub(in crate::flow::engine) fn step_vote_judge(
    state: &FlowState,
    candidate_count: usize,
    k: u32,
) -> Vec<Action> {
    if candidate_count == 0 {
        return emit_and_terminate(FlowOutcome {
            ok: false,
            results: Vec::new(),
            summary: "vote_judge: no candidates".to_string(),
        });
    }
    let candidates: Vec<NodeId> = (0..candidate_count).map(NodeId::candidate).collect();
    // Phase 1: dispatch the candidate wave (all at once) if any is undispatched.
    if let Some(actions) = dispatch_wave(state, &candidates) {
        return actions;
    }
    // Phase 2: wait for every candidate (declared-order fold), then tally.
    let Some(results) = collect_results(state, &candidates) else {
        return Vec::new(); // candidates still running; wait
    };
    let ok = results.iter().filter(|r| r.ok).count() as u32;
    let total = candidate_count as u32;
    let reached = ok >= k;
    let tally = Action::Decision {
        node: NodeId::judge(),
        kind: FlowDecisionKind::VoteTally {
            ok,
            total,
            k,
            reached,
        },
    };
    if !reached {
        // Quorum short: do not run the judge. The tally records WHY (the audit trail);
        // the outcome keeps the candidates so the failure is inspectable.
        let mut actions = vec![tally];
        actions.extend(emit_and_terminate(short_quorum_outcome(results, ok, k)));
        return actions;
    }
    // Phase 3 / 4: drive the judge. The tally fires once, co-emitted with the judge's
    // first dispatch.
    let judge = NodeId::judge();
    match state.result(&judge) {
        // Phase 4: the judge finished — emit its pick + the final outcome.
        Some(judge_result) => {
            let pick = Action::Decision {
                node: judge.clone(),
                kind: FlowDecisionKind::JudgePick {
                    node_id: judge.as_str().to_string(),
                    ok: judge_result.ok,
                },
            };
            let mut actions = vec![pick];
            actions.extend(emit_and_terminate(judged_outcome(judge_result.clone(), ok)));
            actions
        }
        // Judge running (already dispatched): wait.
        None if state.is_dispatched(&judge) => Vec::new(),
        // Phase 3: dispatch the judge, co-emitting the (once-only) tally decision.
        None => vec![
            tally,
            Action::StartWorker {
                node: judge,
                step_index: 0,
            },
        ],
    }
}

/// The outcome when the quorum was reached and the judge picked: the judge's result
/// is the vote's answer (kept first), and the summary records the tally.
fn judged_outcome(judge_result: TurnResult, ok: u32) -> FlowOutcome {
    FlowOutcome {
        ok: judge_result.ok,
        summary: format!(
            "vote_judge: {ok} candidate(s) ok, judge {}",
            if judge_result.ok { "ok" } else { "failed" }
        ),
        results: vec![judge_result],
    }
}

/// The outcome when the quorum was short (fewer than `k` candidate successes): not ok,
/// keeping the candidate results so the shortfall is inspectable.
fn short_quorum_outcome(results: Vec<TurnResult>, ok: u32, k: u32) -> FlowOutcome {
    FlowOutcome {
        ok: false,
        summary: format!("vote_judge: quorum short ({ok}/{k} ok), judge not run"),
        results,
    }
}
