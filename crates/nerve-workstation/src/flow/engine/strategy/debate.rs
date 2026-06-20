//! `Strategy::Debate` — multi-round argument between sides, adjudicated by a judge.
//!
//! The phase machine (design §3):
//!
//! 1. **Rounds** — for each round `0..rounds`, dispatch every `side` for that round in
//!    ONE parallel wave (node id `side-{s}-round-{r}`). Each side's task interpolates
//!    the PRIOR round's arguments from the ledger blackboard (e.g. `{{side-0-round-0}}`
//!    in round 1), resolved in the driver's `build_task` — so a later round genuinely
//!    sees the earlier round's arguments.
//! 2. **Round decision** — once a round's sides all have recorded results, emit a
//!    `DebateRound` audit decision (how many sides argued ok), co-emitted ONCE with the
//!    NEXT phase's dispatch (the next round, or the judge after the last round).
//! 3. **Judge** — after the final round, dispatch the single `judge` (it interpolates
//!    the last round's arguments). Emit a `JudgePick` decision; the judge's result is
//!    the debate's verdict.
//!
//! Bounded by the budget (C3b): each side-turn is a spawn the `FleetBudget` gates, so
//! a deep debate self-terminates on overrun like any other fan-out.

use super::super::{FlowOutcome, FlowState, NodeId};
use super::{collect_results, dispatch_wave, emit_and_terminate};
use crate::flow::Action;
use crate::worker::TurnResult;
use nerve_runtime::FlowDecisionKind;

/// Interpret one step of a `Debate` (design §3). `side_count` sides argue for `rounds`
/// rounds, then a judge adjudicates.
pub(in crate::flow::engine) fn step_debate(
    state: &FlowState,
    side_count: usize,
    rounds: u32,
) -> Vec<Action> {
    if side_count == 0 || rounds == 0 {
        return emit_and_terminate(FlowOutcome {
            ok: false,
            results: Vec::new(),
            summary: format!("debate: nothing to debate ({side_count} sides, {rounds} rounds)"),
        });
    }
    // Walk the rounds in declared order. A round that is fully finished is "settled":
    // advance past it (its decision was already co-emitted when its successor was
    // dispatched). The first not-yet-settled round is the frontier: dispatch it (if
    // undispatched) or wait. Once every round is settled, drive the judge.
    let turns_for = |round: u32| -> Vec<NodeId> {
        (0..side_count)
            .map(|side| NodeId::debate_turn(side, round as usize))
            .collect()
    };
    for round in 0..rounds {
        let turns = turns_for(round);
        // Dispatch this round's wave if any side is undispatched (the frontier).
        if let Some(actions) = dispatch_wave(state, &turns) {
            return actions;
        }
        // Wait for every side this round (declared-order fold).
        let Some(results) = collect_results(state, &turns) else {
            return Vec::new(); // round still running; wait
        };
        // This round is settled. Its `DebateRound` decision is co-emitted ONCE with the
        // NEXT phase's dispatch (so it fires exactly once). If the NEXT round is not yet
        // dispatched, dispatch it now (with the decision prepended) and return; if the
        // next round IS already dispatched, this round was already settled on a prior
        // step — continue to evaluate the frontier without re-emitting.
        let sides_ok = results.iter().filter(|r| r.ok).count() as u32;
        let decision = Action::Decision {
            node: NodeId::judge(),
            kind: FlowDecisionKind::DebateRound { round, sides_ok },
        };
        if round + 1 < rounds {
            let next = turns_for(round + 1);
            if let Some(mut actions) = dispatch_wave(state, &next) {
                let mut out = vec![decision];
                out.append(&mut actions);
                return out;
            }
            // Next round already dispatched — this round is old news; advance.
            continue;
        }
        // Last round settled: drive the judge. The final round's decision fires once,
        // co-emitted with the judge's first dispatch (judge_phase handles that), so we
        // only prepend it when the judge has not yet been dispatched.
        return judge_phase(state, decision);
    }
    Vec::new()
}

/// The judge phase after the last debate round: dispatch the judge (co-emitting the
/// final round's `DebateRound` decision once), wait, then emit the `JudgePick` + the
/// verdict.
fn judge_phase(state: &FlowState, final_round_decision: Action) -> Vec<Action> {
    let judge = NodeId::judge();
    match state.result(&judge) {
        Some(verdict) => {
            let pick = Action::Decision {
                node: judge.clone(),
                kind: FlowDecisionKind::JudgePick {
                    node_id: judge.as_str().to_string(),
                    ok: verdict.ok,
                },
            };
            let mut actions = vec![pick];
            actions.extend(emit_and_terminate(verdict_outcome(verdict.clone())));
            actions
        }
        None if state.is_dispatched(&judge) => Vec::new(),
        None => vec![
            final_round_decision,
            Action::StartWorker {
                node: judge,
                step_index: 0,
            },
        ],
    }
}

/// The outcome of a finished debate: the judge's verdict is the answer (kept).
fn verdict_outcome(verdict: TurnResult) -> FlowOutcome {
    FlowOutcome {
        ok: verdict.ok,
        summary: format!(
            "debate: judge {}",
            if verdict.ok { "ruled" } else { "failed" }
        ),
        results: vec![verdict],
    }
}
