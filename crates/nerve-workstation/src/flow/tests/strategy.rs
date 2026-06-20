//! GOLDEN + byte-identical REPLAY tests for the richer C5 strategies (design §3 / §10
//! Wave C5): `VoteJudge`, `MapReduce`, `Debate`, `Hierarchical`.
//!
//! Each strategy gets (a) a GOLDEN insta snapshot of its aggregated outcome over
//! scripted [`FakeWorker`](super::FakeWorker)s (incl. the interesting cases the design
//! calls out — a vote tie, a debate to round N, a map over 3 items, a hierarchical
//! depth-limit refusal), (b) a byte-identical REPLAY assertion reusing the C4 production
//! [`ReplayResolver`](crate::flow::ReplayResolver) gate, and (c) where it matters, the
//! `FlowDecision` audit trail captured via a [`DecisionObserver`]. A mixed-substrate
//! case (CLI candidates + an in-process provider judge) is exercised too — the engine
//! sees only `AgentWorker`, so the substrate mix is invisible to the control flow.

use super::{
    FakeResolver, NeverApprover, Script, cli_step, def, fail, ok, provider_step, record,
    render_outcome, script,
};
use crate::delegate_proxy::DelegateApprover;
use crate::flow::{Driver, FlowObserver, FlowOutcome, ReplayResolver, replay_generation_provider};
use crate::worker::{SpawnRefusal, TurnResult, WorkerLedger};
use nerve_core::CancelToken;
use nerve_runtime::{ContextSplit, FlowDecisionKind, Step, Strategy, WorkflowDef};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

// ---- Decision-capturing observer (the audit trail) -----------------------------

/// A [`FlowObserver`] that captures every interpreter `decision` (and spawn refusal),
/// so a test can assert the exact `FlowDecision` audit trail (vote tally / judge pick /
/// debate round / depth ceiling) the host would emit. Node/budget callbacks are no-ops.
#[derive(Default)]
struct DecisionObserver {
    decisions: Mutex<Vec<FlowDecisionKind>>,
    refusals: Mutex<Vec<(String, SpawnRefusal)>>,
}

impl FlowObserver for DecisionObserver {
    fn node_started(&self, _node: &str, _worker: &nerve_runtime::WorkerRef) {}
    fn node_finished(&self, _node: &str, _result: &TurnResult) {}
    fn spawn_refused(&self, node: &str, refusal: SpawnRefusal) {
        crate::sync::lock_recover(&self.refusals).push((node.to_string(), refusal));
    }
    fn decision(&self, _node: &str, kind: &FlowDecisionKind) {
        crate::sync::lock_recover(&self.decisions).push(kind.clone());
    }
}

impl DecisionObserver {
    fn decisions(&self) -> Vec<FlowDecisionKind> {
        crate::sync::lock_recover(&self.decisions).clone()
    }
}

/// Record `def` over `scripts` with a [`DecisionObserver`] attached, returning the
/// outcome, the recorded ledger, and the captured decisions.
fn record_with_decisions(
    def: &WorkflowDef,
    scripts: BTreeMap<String, Script>,
) -> (FlowOutcome, Arc<WorkerLedger>, Vec<FlowDecisionKind>) {
    let scripts = Arc::new(scripts);
    let resolver = FakeResolver::new(Arc::clone(&scripts));
    let ledger = Arc::new(WorkerLedger::new());
    let observer = DecisionObserver::default();
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_concurrency(8)
        .with_observer(&observer);
    let outcome = driver.run(def, &CancelToken::never());
    (outcome, ledger, observer.decisions())
}

/// Replay a recorded ledger through the PRODUCTION resolver + recorded-generation
/// provider, asserting the replayed ledger is byte-identical to the recorded one and
/// the outcome matches (the C4 audit gate, now over a richer strategy).
fn assert_replay_byte_identical(
    workflow: &WorkflowDef,
    recorded_outcome: &FlowOutcome,
    recorded: &WorkerLedger,
) {
    let recorded_jsonl = recorded.to_jsonl();
    let resolver = ReplayResolver::from_ledger(recorded);
    let generation = replay_generation_provider(recorded);
    let replay_ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let replay_outcome = Driver::new(&resolver, Arc::clone(&replay_ledger), approver, None)
        .with_concurrency(8)
        .with_generation(&generation)
        .run(workflow, &CancelToken::never());
    assert_eq!(
        render_outcome(&replay_outcome),
        render_outcome(recorded_outcome),
        "replay must reproduce the recorded outcome exactly"
    );
    assert_eq!(
        replay_ledger.to_jsonl(),
        recorded_jsonl,
        "replayed ledger must be byte-identical to the recorded ledger (the audit gate)"
    );
}

// ================================ VoteJudge ====================================

/// A `VoteJudge` with three CLI candidates and an in-process PROVIDER judge (the
/// mixed-substrate, design §7). The judge's task interpolates the candidate outputs.
fn vote_judge_def(k: u32) -> WorkflowDef {
    def(
        "vote",
        Strategy::VoteJudge {
            candidates: vec![cli_step("cand A"), cli_step("cand B"), cli_step("cand C")],
            judge: provider_step("judge over {{cand-0}} / {{cand-1}} / {{cand-2}}"),
            k,
        },
    )
}

#[test]
fn golden_vote_judge_quorum_reached_then_judge_picks() {
    let workflow = vote_judge_def(2);
    let scripts = BTreeMap::from([
        ("cand A".to_string(), script(ok("A"))),
        ("cand B".to_string(), script(fail("B failed"))),
        ("cand C".to_string(), script(ok("C"))),
        (
            "judge over A / B failed / C".to_string(),
            script(ok("VERDICT: C")),
        ),
    ]);
    let (outcome, ledger, decisions) = record_with_decisions(&workflow, scripts);
    // The audit trail: a tally (2/3 ok, quorum 2 reached) then the judge's pick.
    assert_eq!(
        decisions,
        vec![
            FlowDecisionKind::VoteTally {
                ok: 2,
                total: 3,
                k: 2,
                reached: true,
            },
            FlowDecisionKind::JudgePick {
                node_id: "judge".into(),
                ok: true,
            },
        ]
    );
    insta::assert_snapshot!("golden_vote_judge", render_outcome(&outcome));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn golden_vote_judge_tie_still_runs_the_judge() {
    // The interesting case: a TIE (1 ok / 1 ok) with quorum k=1. Both viable
    // candidates reach the judge, which breaks the tie. The tally records reached.
    let workflow = def(
        "vote-tie",
        Strategy::VoteJudge {
            candidates: vec![cli_step("opt X"), cli_step("opt Y")],
            judge: provider_step("break the tie: {{cand-0}} vs {{cand-1}}"),
            k: 1,
        },
    );
    let scripts = BTreeMap::from([
        ("opt X".to_string(), script(ok("X"))),
        ("opt Y".to_string(), script(ok("Y"))),
        (
            "break the tie: X vs Y".to_string(),
            script(ok("TIE BROKEN: X")),
        ),
    ]);
    let (outcome, ledger, decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(outcome.results[0].text, "TIE BROKEN: X");
    assert_eq!(
        decisions[0],
        FlowDecisionKind::VoteTally {
            ok: 2,
            total: 2,
            k: 1,
            reached: true,
        }
    );
    insta::assert_snapshot!("golden_vote_judge_tie", render_outcome(&outcome));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn vote_judge_short_quorum_skips_the_judge() {
    // Only 1 candidate ok but k=2 → quorum SHORT: the judge is NOT run; the tally
    // records why; the outcome keeps the candidates (not ok).
    let workflow = vote_judge_def(2);
    let scripts = BTreeMap::from([
        ("cand A".to_string(), script(ok("A"))),
        ("cand B".to_string(), script(fail("B failed"))),
        ("cand C".to_string(), script(fail("C failed"))),
    ]);
    let (outcome, _ledger, decisions) = record_with_decisions(&workflow, scripts);
    assert!(!outcome.ok, "a short quorum is not ok");
    assert_eq!(
        decisions,
        vec![FlowDecisionKind::VoteTally {
            ok: 1,
            total: 3,
            k: 2,
            reached: false,
        }],
        "only the tally fires; the judge never ran, so no JudgePick"
    );
    assert_eq!(outcome.results.len(), 3, "candidates kept for inspection");
}

// ================================ MapReduce ====================================

#[test]
fn golden_map_reduce_over_three_shards() {
    // The interesting case: map over 3 shards (Shards { n: 3 }), then reduce. Each map
    // worker sees its own `{{split}}`; the reduce interpolates the map outputs.
    let workflow = def(
        "mapreduce",
        Strategy::MapReduce {
            map: cli_step("summarize {{split}}"),
            over: ContextSplit::Shards { n: 3 },
            reduce: provider_step("merge {{map-0}} + {{map-1}} + {{map-2}}"),
        },
    );
    let scripts = BTreeMap::from([
        ("summarize shard 0/3".to_string(), script(ok("S0"))),
        ("summarize shard 1/3".to_string(), script(ok("S1"))),
        ("summarize shard 2/3".to_string(), script(ok("S2"))),
        ("merge S0 + S1 + S2".to_string(), script(ok("MERGED"))),
    ]);
    let (outcome, ledger, _decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(outcome.results[0].text, "MERGED");
    insta::assert_snapshot!("golden_map_reduce", render_outcome(&outcome));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn golden_map_reduce_over_path_groups() {
    // The `Paths` split: one map worker per declared path group; `{{split}}` is the
    // joined paths. Reduce over the two map outputs.
    let workflow = def(
        "mapreduce-paths",
        Strategy::MapReduce {
            map: cli_step("review {{split}}"),
            over: ContextSplit::Paths {
                groups: vec![
                    vec!["src/a.rs".into(), "src/b.rs".into()],
                    vec!["src/c.rs".into()],
                ],
            },
            reduce: provider_step("combine {{map-0}} & {{map-1}}"),
        },
    );
    let scripts = BTreeMap::from([
        ("review src/a.rs, src/b.rs".to_string(), script(ok("R0"))),
        ("review src/c.rs".to_string(), script(ok("R1"))),
        ("combine R0 & R1".to_string(), script(ok("COMBINED"))),
    ]);
    let (outcome, ledger) = record(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(outcome.results[0].text, "COMBINED");
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

// ================================== Debate =====================================

/// A 2-side, `rounds`-round debate with an in-process provider judge. Round 1+ sides
/// interpolate the prior round's arguments (`{{side-0-round-0}}`).
fn debate_def(rounds: u32) -> WorkflowDef {
    def(
        "debate",
        Strategy::Debate {
            sides: vec![
                cli_step("argue pro (prior: {{side-0-round-0}}{{side-1-round-0}})"),
                cli_step("argue con (prior: {{side-0-round-0}}{{side-1-round-0}})"),
            ],
            rounds,
            judge: provider_step("rule on {{side-0-round-1}} vs {{side-1-round-1}}"),
        },
    )
}

#[test]
fn golden_debate_to_round_two_then_judge() {
    // The interesting case: a debate to round N (here 2 rounds), where round 1 sees
    // round 0's arguments, then the judge rules. The per-round decisions + judge pick
    // form the audit trail.
    let workflow = debate_def(2);
    // Round 0: the prior-round placeholders are unresolved (stay verbatim) → the
    // scripts key on the literal round-0 prompt.
    let round0 = "argue pro (prior: {{side-0-round-0}}{{side-1-round-0}})";
    let round0_con = "argue con (prior: {{side-0-round-0}}{{side-1-round-0}})";
    // Round 1: side prompts interpolate round 0's outputs (PRO0 / CON0).
    let round1_pro = "argue pro (prior: PRO0CON0)";
    let round1_con = "argue con (prior: PRO0CON0)";
    let scripts = BTreeMap::from([
        (round0.to_string(), script(ok("PRO0"))),
        (round0_con.to_string(), script(ok("CON0"))),
        (round1_pro.to_string(), script(ok("PRO1"))),
        (round1_con.to_string(), script(ok("CON1"))),
        ("rule on PRO1 vs CON1".to_string(), script(ok("RULING"))),
    ]);
    let (outcome, ledger, decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(outcome.results[0].text, "RULING");
    // The audit trail: one DebateRound per round, then the judge's pick.
    assert_eq!(
        decisions,
        vec![
            FlowDecisionKind::DebateRound {
                round: 0,
                sides_ok: 2,
            },
            FlowDecisionKind::DebateRound {
                round: 1,
                sides_ok: 2,
            },
            FlowDecisionKind::JudgePick {
                node_id: "judge".into(),
                ok: true,
            },
        ]
    );
    insta::assert_snapshot!("golden_debate", render_outcome(&outcome));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

// =============================== Hierarchical ==================================

/// A `Hierarchical` whose planner runs, then a child `Single` flow.
fn hierarchical_def(max_depth: u32, child: Strategy) -> WorkflowDef {
    let mut workflow = def(
        "hier",
        Strategy::Hierarchical {
            planner: provider_step("plan the work"),
            child: Box::new(child),
        },
    );
    workflow.max_depth = max_depth;
    workflow
}

#[test]
fn golden_hierarchical_planner_then_child_flow() {
    // max_depth 2: the child flow (at depth 1) is below the ceiling, so it runs. The
    // child's node id is `child/node-0` (the shared-ledger namespace).
    let workflow = hierarchical_def(
        2,
        Strategy::Single {
            step: cli_step("do the planned work"),
        },
    );
    let scripts = BTreeMap::from([
        ("plan the work".to_string(), script(ok("PLAN"))),
        ("do the planned work".to_string(), script(ok("DONE"))),
    ]);
    let (outcome, ledger, decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    // The planner leads, the child's result trails.
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>(),
        vec!["PLAN", "DONE"],
    );
    // No depth refusal at max_depth 2 (the child ran).
    assert!(
        decisions.is_empty(),
        "no depth-ceiling decision: {decisions:?}"
    );
    // The child node recorded under `child/node-0` in the SHARED ledger.
    assert_eq!(ledger.output("child/node-0"), Some("DONE".to_string()));
    insta::assert_snapshot!("golden_hierarchical", render_outcome(&outcome));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn golden_hierarchical_depth_limit_refuses_the_child() {
    // The interesting case: max_depth 1. The child flow would run at depth 1, which is
    // NOT below the ceiling (1 >= 1), so the engine REFUSES it — a recorded
    // DepthCeiling decision (absence-at-floor), not a crash — and the planner's result
    // is the answer. The child worker never runs.
    let workflow = hierarchical_def(
        1,
        Strategy::Single {
            step: cli_step("do the planned work"),
        },
    );
    let scripts = BTreeMap::from([
        ("plan the work".to_string(), script(ok("PLAN ONLY"))),
        // The child script exists but must never be reached.
        (
            "do the planned work".to_string(),
            script(ok("SHOULD NOT RUN")),
        ),
    ]);
    let (outcome, ledger, decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok, "the planner ran ok; the flow is ok");
    assert_eq!(outcome.results.len(), 1, "planner only");
    assert_eq!(outcome.results[0].text, "PLAN ONLY");
    assert_eq!(
        decisions,
        vec![FlowDecisionKind::DepthCeiling {
            depth: 1,
            max_depth: 1,
        }],
    );
    // The child worker was never dispatched (no `child/node-0` on the tape).
    assert_eq!(ledger.output("child/node-0"), None);
    insta::assert_snapshot!("golden_hierarchical_depth_limit", render_outcome(&outcome));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn hierarchical_child_parallel_records_prefixed_branch_ids() {
    // The child flow can be ANY strategy; here a Parallel, so the child branches record
    // under `child/branch-0` / `child/branch-1` (nested namespace), and replay stays
    // byte-identical.
    let workflow = hierarchical_def(
        2,
        Strategy::Parallel {
            branches: vec![cli_step("sub task A"), cli_step("sub task B")],
            join: nerve_runtime::Join::All,
        },
    );
    let scripts = BTreeMap::from([
        ("plan the work".to_string(), script(ok("PLAN"))),
        ("sub task A".to_string(), script(ok("subA"))),
        ("sub task B".to_string(), script(ok("subB"))),
    ]);
    let (outcome, ledger, _decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(ledger.output("child/branch-0"), Some("subA".to_string()));
    assert_eq!(ledger.output("child/branch-1"), Some("subB".to_string()));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn hierarchical_child_mapreduce_resolves_nested_split() {
    // Finding L: a Hierarchical whose child is a MapReduce. Each nested map node
    // (`child/map-i`) must resolve its own `{{split}}` shard — previously the map-split
    // resolver ignored the `child/` prefix and rendered `{{split}}` to nothing, so the
    // shard prompts never matched their scripts. Now the nested map node finds shard i.
    // The reduce uses a fixed prompt (cross-node `{{map-i}}` interpolation under the
    // nested namespace is a separate concern); this test isolates the `{{split}}` fix —
    // each nested map node must resolve its OWN shard, which previously rendered empty.
    let mut workflow = def(
        "hier-mr",
        Strategy::Hierarchical {
            planner: provider_step("plan the shards"),
            child: Box::new(Strategy::MapReduce {
                map: cli_step("summarize {{split}}"),
                over: ContextSplit::Shards { n: 2 },
                reduce: provider_step("merge the shards"),
            }),
        },
    );
    workflow.max_depth = 3; // child MapReduce runs at depth 1, below the ceiling
    let scripts = BTreeMap::from([
        ("plan the shards".to_string(), script(ok("PLAN"))),
        // The nested map nodes interpolate their OWN shard via {{split}} — before the
        // fix these rendered to `summarize ` (empty split), missing these scripts.
        ("summarize shard 0/2".to_string(), script(ok("S0"))),
        ("summarize shard 1/2".to_string(), script(ok("S1"))),
        ("merge the shards".to_string(), script(ok("MERGED"))),
    ]);
    let (outcome, ledger, _decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok, "the nested map/reduce ran: {outcome:?}");
    // The nested map nodes resolved their shards (recorded under the child namespace) —
    // proof the `child/map-i` split now resolves (finding L).
    assert_eq!(ledger.output("child/map-0"), Some("S0".to_string()));
    assert_eq!(ledger.output("child/map-1"), Some("S1".to_string()));
    assert_eq!(ledger.output("child/reduce"), Some("MERGED".to_string()));
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

// ---- Mixed-substrate: CLI candidates + an in-process provider judge ------------

#[test]
fn vote_judge_mixes_cli_candidates_with_a_provider_judge() {
    // Design §7: candidates run as CLI workers, the judge as an in-process provider —
    // the engine sees only `AgentWorker`, so the control flow is substrate-agnostic.
    // The FakeResolver hands cli refs a Cli-kind fake and provider refs a Provider-kind
    // fake, so this exercises the mixed kinds hermetically.
    let candidates: Vec<Step> = vec![cli_step("cli cand 0"), cli_step("cli cand 1")];
    let workflow = def(
        "mixed",
        Strategy::VoteJudge {
            candidates,
            judge: provider_step("provider judge: {{cand-0}} {{cand-1}}"),
            k: 2,
        },
    );
    let scripts = BTreeMap::from([
        ("cli cand 0".to_string(), script(ok("c0"))),
        ("cli cand 1".to_string(), script(ok("c1"))),
        ("provider judge: c0 c1".to_string(), script(ok("PICKED c0"))),
    ]);
    let (outcome, ledger, _decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(outcome.results[0].text, "PICKED c0");
    // Byte-identical replay over the mixed-substrate tape.
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

#[test]
fn nested_hierarchical_depth_gate_composes() {
    // A Hierarchical whose child is ITSELF Hierarchical (distinct planner prompts so
    // it is not a fork-loop). With max_depth 2: the root planner (depth 0) spawns the
    // child Hierarchical (depth 1, below the ceiling), whose planner runs; but its
    // grandchild flow would be at depth 2 (== max_depth) → REFUSED. So we see two
    // planners run and ONE depth-ceiling refusal, recorded under the nested namespace.
    let mut workflow = def(
        "nested-hier",
        Strategy::Hierarchical {
            planner: provider_step("root plan"),
            child: Box::new(Strategy::Hierarchical {
                planner: provider_step("child plan"),
                child: Box::new(Strategy::Single {
                    step: cli_step("grandchild work"),
                }),
            }),
        },
    );
    workflow.max_depth = 2;
    let scripts = BTreeMap::from([
        ("root plan".to_string(), script(ok("ROOT"))),
        ("child plan".to_string(), script(ok("CHILD"))),
        // The grandchild worker must never run (its flow is at the ceiling).
        ("grandchild work".to_string(), script(ok("SHOULD NOT RUN"))),
    ]);
    let (outcome, ledger, decisions) = record_with_decisions(&workflow, scripts);
    assert!(outcome.ok);
    // Two planners ran; the grandchild flow was refused at depth 2.
    assert_eq!(ledger.output("planner"), Some("ROOT".to_string()));
    assert_eq!(ledger.output("child/planner"), Some("CHILD".to_string()));
    assert_eq!(
        ledger.output("child/child/node-0"),
        None,
        "grandchild never ran"
    );
    assert_eq!(
        decisions,
        vec![FlowDecisionKind::DepthCeiling {
            depth: 2,
            max_depth: 2,
        }],
    );
    // Byte-identical replay over the nested tape (the nested namespace round-trips).
    assert_replay_byte_identical(&workflow, &outcome, &ledger);
}

// ---- Finding H: child autonomy is clamped to the parent planner's --------------

/// A worker that records the EFFECTIVE autonomy it was handed (keyed by node id), so a
/// test can prove a Hierarchical child's autonomy was intersected with its parent's.
struct AutonomyProbe {
    seen: Arc<Mutex<std::collections::BTreeMap<String, nerve_runtime::DelegateAutonomy>>>,
}

impl crate::worker::AgentWorker for AutonomyProbe {
    fn kind(&self) -> crate::worker::WorkerKind {
        crate::worker::WorkerKind::Cli("probe")
    }
    fn capability(&self) -> nerve_runtime::RiskTier {
        nerve_runtime::RiskTier::Edit
    }
    fn start(
        &self,
        task: &crate::worker::WorkerTask,
        _ctx: &crate::worker::WorkerContext,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(crate::worker::WorkerEvent),
    ) -> Result<Box<dyn crate::worker::WorkerSession>, crate::worker::WorkerError> {
        crate::sync::lock_recover(&self.seen).insert(task.node_id.clone(), task.autonomy);
        let result = ok(&format!("did {}", task.prompt));
        crate::worker::synthesize_turn_steps(1, &result, on_event);
        Ok(Box::new(AutonomyProbeSession { last: result }))
    }
}

struct AutonomyProbeSession {
    last: TurnResult,
}
impl crate::worker::WorkerSession for AutonomyProbeSession {
    fn steer(
        &mut self,
        _m: &str,
        _c: &CancelToken,
        _e: &mut dyn FnMut(crate::worker::WorkerEvent),
    ) -> Result<TurnResult, crate::worker::WorkerError> {
        Err(crate::worker::WorkerError::NotSteerable)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

struct AutonomyResolver {
    seen: Arc<Mutex<std::collections::BTreeMap<String, nerve_runtime::DelegateAutonomy>>>,
}
impl crate::flow::WorkerResolver for AutonomyResolver {
    fn resolve(
        &self,
        _w: &nerve_runtime::WorkerRef,
    ) -> Result<Box<dyn crate::worker::AgentWorker>, crate::worker::WorkerError> {
        Ok(Box::new(AutonomyProbe {
            seen: Arc::clone(&self.seen),
        }))
    }
}

/// A `Hierarchical` step with an explicit autonomy (so a test can give the planner a
/// NARROW autonomy and the child a WIDER one, proving the child is clamped down).
fn step_with_autonomy(prompt: &str, autonomy: nerve_runtime::DelegateAutonomy) -> Step {
    Step {
        worker: nerve_runtime::WorkerRef::Cli {
            name: "claude".into(),
        },
        task: nerve_runtime::TaskTemplate::new(prompt),
        autonomy,
        on_fail: nerve_runtime::FailPolicy::Continue,
    }
}

#[test]
fn hierarchical_child_autonomy_is_clamped_to_the_parent_planner() {
    use nerve_runtime::DelegateAutonomy::{Edit, Full, ReadOnly};
    // Finding H: a child node declared with WIDER autonomy than its parent planner is
    // clamped to the parent (monotone de-escalation). The planner here is Edit; the
    // child Single declares Full, but must run with min(Full, Edit) = Edit.
    let mut workflow = def(
        "hier-autonomy",
        Strategy::Hierarchical {
            planner: step_with_autonomy("plan", Edit),
            child: Box::new(Strategy::Single {
                step: step_with_autonomy("do work", Full), // WIDER than the parent
            }),
        },
    );
    workflow.max_depth = 3;

    let seen = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let resolver = AutonomyResolver {
        seen: Arc::clone(&seen),
    };
    let ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    Driver::new(&resolver, ledger, approver, None)
        .with_concurrency(4)
        .run(&workflow, &CancelToken::never());

    let seen = crate::sync::lock_recover(&seen).clone();
    assert_eq!(
        seen.get("planner"),
        Some(&Edit),
        "the planner runs with its own declared autonomy"
    );
    assert_eq!(
        seen.get("child/node-0"),
        Some(&Edit),
        "the child declared Full but is clamped to the parent planner's Edit"
    );

    // And a child that declares NARROWER autonomy keeps its tighter posture (the clamp
    // never WIDENS): a ReadOnly child under an Edit planner stays ReadOnly.
    let mut narrow = def(
        "hier-autonomy-narrow",
        Strategy::Hierarchical {
            planner: step_with_autonomy("plan", Edit),
            child: Box::new(Strategy::Single {
                step: step_with_autonomy("do work", ReadOnly),
            }),
        },
    );
    narrow.max_depth = 3;
    let seen2 = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let resolver2 = AutonomyResolver {
        seen: Arc::clone(&seen2),
    };
    Driver::new(
        &resolver2,
        Arc::new(WorkerLedger::new()),
        Arc::new(NeverApprover),
        None,
    )
    .with_concurrency(4)
    .run(&narrow, &CancelToken::never());
    assert_eq!(
        crate::sync::lock_recover(&seen2).get("child/node-0"),
        Some(&ReadOnly),
        "a narrower child keeps its tighter autonomy (the clamp never widens)"
    );
}

#[test]
fn nested_hierarchical_child_clamps_against_every_ancestor_planner() {
    use nerve_runtime::DelegateAutonomy::{Full, ReadOnly};
    // A nested Hierarchical: root planner Full, child planner ReadOnly, grandchild work
    // declares Full. The grandchild's effective autonomy is min(Full, Full, ReadOnly) =
    // ReadOnly — it clamps against EVERY ancestor planner on its path, not just one.
    let mut workflow = def(
        "nested-autonomy",
        Strategy::Hierarchical {
            planner: step_with_autonomy("root plan", Full),
            child: Box::new(Strategy::Hierarchical {
                planner: step_with_autonomy("child plan", ReadOnly),
                child: Box::new(Strategy::Single {
                    step: step_with_autonomy("grandchild work", Full),
                }),
            }),
        },
    );
    workflow.max_depth = 4;
    let seen = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let resolver = AutonomyResolver {
        seen: Arc::clone(&seen),
    };
    Driver::new(
        &resolver,
        Arc::new(WorkerLedger::new()),
        Arc::new(NeverApprover),
        None,
    )
    .with_concurrency(4)
    .run(&workflow, &CancelToken::never());
    let seen = crate::sync::lock_recover(&seen).clone();
    assert_eq!(seen.get("planner"), Some(&Full));
    assert_eq!(seen.get("child/planner"), Some(&ReadOnly));
    assert_eq!(
        seen.get("child/child/node-0"),
        Some(&ReadOnly),
        "the grandchild clamps against EVERY ancestor planner (min of all)"
    );
}

// ---- Static safety (design §8): fork-loop + zero-depth rejected at flow.start ---

#[test]
fn validate_rejects_a_planner_fork_loop() {
    // Two nested Hierarchical planners with the IDENTICAL instruction is a fork-loop —
    // rejected at flow.start before any worker spawns (the ancestor-instruction-hash
    // guard, design §8).
    let mut workflow = def(
        "loop",
        Strategy::Hierarchical {
            planner: provider_step("plan recursively"),
            child: Box::new(Strategy::Hierarchical {
                planner: provider_step("plan recursively"), // SAME instruction → loop
                child: Box::new(Strategy::Single {
                    step: cli_step("work"),
                }),
            }),
        },
    );
    workflow.max_depth = 5;
    assert_eq!(
        crate::flow::validate_workflow(&workflow),
        Err(crate::flow::WorkflowError::PlannerForkLoop { depth: 1 }),
    );
}
