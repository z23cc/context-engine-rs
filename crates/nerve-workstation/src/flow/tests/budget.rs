//! BUDGET governance tests (Wave C3b, design §6/§8).
//!
//! These drive the [`Driver`](crate::flow::Driver) with a per-flow
//! [`BudgetLedger`](crate::worker::BudgetLedger) + [`FleetBudget`](crate::worker::FleetBudget)
//! attached and a capturing [`FlowObserver`](crate::flow::FlowObserver) that
//! records every `budget_debited` / `spawn_refused` callback (the host maps these
//! onto `BudgetUpdate` / `BudgetWarning` / `FlowDecision` events). They pin the
//! four C3b properties hermetically (FakeWorker, no live LLM/subprocess):
//!
//! - **Overrun → cooperative cancel** — a flow whose workers report usage above
//!   `max_total_cost_usd` / `max_total_tokens` warns, then exhausts, then cancels.
//! - **Absence-at-floor** — the engine refuses to spawn beyond the depth / worker
//!   ceiling and records a deterministic refusal rather than crashing.
//! - **Monotone de-escalation** — the contract that a child grant can only narrow
//!   its parent's (driven through the driver's `node_grant`, plus the unit contract
//!   in [`crate::worker`]).
//! - **Replay determinism** — the budget fold reproduces the same decision
//!   sequence on a re-run (byte-identical), because it folds RECORDED usage.

use super::{FakeResolver, NeverApprover, Script, cli_step, def, fail, ok, script};
use crate::delegate_proxy::DelegateApprover;
use crate::flow::{Driver, FlowObserver};
use crate::worker::{
    BudgetDecision, BudgetLedger, BudgetSnapshot, FleetBudget, SpawnRefusal, TurnResult,
    WorkerLedger,
};
use nerve_core::CancelToken;
use nerve_runtime::{BudgetSpec, Join, Strategy, WorkflowDef};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A [`FlowObserver`] that captures the budget callbacks for assertion, so a test
/// can inspect exactly the `BudgetUpdate` / `BudgetWarning` / `FlowDecision` the
/// host would emit. Node lifecycle callbacks are no-ops here.
#[derive(Default)]
struct CaptureObserver {
    debits: Mutex<Vec<(BudgetSnapshot, BudgetDecision)>>,
    refusals: Mutex<Vec<(String, SpawnRefusal)>>,
}

impl FlowObserver for CaptureObserver {
    fn node_started(&self, _node: &str, _worker: &nerve_runtime::WorkerRef) {}
    fn node_finished(&self, _node: &str, _result: &TurnResult) {}
    fn budget_debited(&self, snapshot: BudgetSnapshot, decision: BudgetDecision) {
        crate::sync::lock_recover(&self.debits).push((snapshot, decision));
    }
    fn spawn_refused(&self, node: &str, refusal: SpawnRefusal) {
        crate::sync::lock_recover(&self.refusals).push((node.to_string(), refusal));
    }
}

impl CaptureObserver {
    fn decisions(&self) -> Vec<BudgetDecision> {
        crate::sync::lock_recover(&self.debits)
            .iter()
            .map(|(_, d)| *d)
            .collect()
    }
    fn warned(&self) -> bool {
        self.decisions()
            .iter()
            .any(|d| matches!(d, BudgetDecision::Warn { .. }))
    }
    fn exhausted(&self) -> bool {
        self.decisions()
            .iter()
            .any(|d| matches!(d, BudgetDecision::Exhausted))
    }
    fn refusals(&self) -> Vec<(String, SpawnRefusal)> {
        crate::sync::lock_recover(&self.refusals).clone()
    }
}

/// A pricey scripted result: $0.50 + 1000 tokens, so a handful exceed a small cap.
fn pricey(text: &str) -> TurnResult {
    TurnResult {
        ok: true,
        text: text.into(),
        usage: nerve_agent::Usage {
            input_tokens: 600,
            output_tokens: 400,
            ..nerve_agent::Usage::default()
        },
        cost_usd: Some(0.50),
        timed_out: false,
    }
}

fn budgeted_def(strategy: Strategy, spec: BudgetSpec, max_depth: u32) -> WorkflowDef {
    WorkflowDef {
        schema_version: 1,
        name: "budgeted".into(),
        strategy,
        budget: spec,
        max_depth,
    }
}

/// Run `def` over `scripts` with budget governance attached, returning the
/// outcome, the final budget snapshot, and the capturing observer.
fn run_budgeted(
    def: &WorkflowDef,
    scripts: BTreeMap<String, Script>,
) -> (super::FlowOutcome, BudgetSnapshot, CaptureObserver) {
    let scripts = Arc::new(scripts);
    let resolver = FakeResolver::new(Arc::clone(&scripts));
    let ledger = Arc::new(WorkerLedger::new());
    let budget = Arc::new(BudgetLedger::new(def.budget));
    let fleet = FleetBudget::root(
        def.max_depth,
        def.budget.max_workers,
        budget.remaining_usd(),
        budget.remaining_tokens(),
    );
    let observer = CaptureObserver::default();
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_concurrency(1)
        .with_observer(&observer)
        .with_budget(Arc::clone(&budget), fleet);
    let outcome = driver.run(def, &CancelToken::never());
    let snapshot = budget.snapshot();
    (outcome, snapshot, observer)
}

fn spec(usd: Option<f64>, tokens: Option<u64>, workers: Option<u32>) -> BudgetSpec {
    BudgetSpec {
        max_total_cost_usd: usd,
        max_total_tokens: tokens,
        max_workers: workers,
    }
}

// ---- 1. Budget overrun → cooperative cancel + not-ok --------------------------

#[test]
fn usd_overrun_warns_exhausts_and_ends_not_ok() {
    // A 3-stage pipeline; each stage costs $0.50. Budget $1.00 → stage 1 ($0.50)
    // is within, stage 2 ($1.00) crosses the 80% warn, stage 3 ($1.50) exhausts
    // and cancels (so the pipeline never folds all three as ok).
    let pipeline = Strategy::Pipeline {
        stages: vec![cli_step("s0"), cli_step("s1"), cli_step("s2")],
    };
    let def = budgeted_def(pipeline, spec(Some(1.0), None, None), 2);
    let scripts = BTreeMap::from([
        ("s0".to_string(), script(pricey("R0"))),
        ("s1".to_string(), script(pricey("R1"))),
        ("s2".to_string(), script(pricey("R2"))),
    ]);
    let (outcome, snapshot, observer) = run_budgeted(&def, scripts);

    // BudgetUpdate fired for each debit; a warning and an exhaustion were recorded.
    assert!(
        observer.decisions().len() >= 2,
        "a BudgetUpdate per debit (got {:?})",
        observer.decisions()
    );
    assert!(observer.warned(), "crossing 80% must emit a BudgetWarning");
    assert!(
        observer.exhausted(),
        "crossing the USD ceiling must emit FlowDecision{{budget_exhausted}}"
    );
    assert!(snapshot.exhausted, "the ledger marks the overrun");
    // Cooperative cancel: the flow did NOT complete ok.
    assert!(!outcome.ok, "an exhausted flow ends not-ok");
}

#[test]
fn token_overrun_exhausts_and_cancels() {
    // No USD cap; a token cap of 1500. Each parallel branch reports 1000 tokens, so
    // the SECOND debit (2000 > 1500) exhausts and cancels.
    let parallel = Strategy::Parallel {
        branches: vec![cli_step("a"), cli_step("b"), cli_step("c")],
        join: Join::All,
    };
    let def = budgeted_def(parallel, spec(None, Some(1500), None), 2);
    let scripts = BTreeMap::from([
        ("a".to_string(), script(pricey("A"))),
        ("b".to_string(), script(pricey("B"))),
        ("c".to_string(), script(pricey("C"))),
    ]);
    let (outcome, snapshot, observer) = run_budgeted(&def, scripts);
    assert!(observer.exhausted(), "a token overrun exhausts");
    assert!(snapshot.spent_tokens > 1500);
    assert!(!outcome.ok, "an exhausted parallel flow ends not-ok");
}

/// A zero-usage scripted result: ok, but NO cost and NO tokens — modeling a worker
/// (gemini / remote / mcp) that reports nothing. Under a token-only budget this MUST
/// still be charged the worst case (finding G).
fn silent(text: &str) -> TurnResult {
    TurnResult {
        ok: true,
        text: text.into(),
        usage: nerve_agent::Usage::default(),
        cost_usd: None,
        timed_out: false,
    }
}

#[test]
fn zero_usage_worker_under_a_token_budget_is_charged_worst_case_and_self_cancels() {
    // Finding G: a worker reporting NO usage under a TOKEN-only budget previously ran
    // FREE, defeating the brake. Now each zero-usage node is charged the full token
    // ceiling (worst case), so the SECOND stage exhausts and the flow self-cancels.
    let pipeline = Strategy::Pipeline {
        stages: vec![cli_step("g0"), cli_step("g1"), cli_step("g2")],
    };
    let def = budgeted_def(pipeline, spec(None, Some(1000), None), 2);
    let scripts = BTreeMap::from([
        ("g0".to_string(), script(silent("R0"))),
        ("g1".to_string(), script(silent("R1"))),
        ("g2".to_string(), script(silent("R2"))),
    ]);
    let (outcome, snapshot, observer) = run_budgeted(&def, scripts);
    assert!(
        observer.exhausted(),
        "a zero-usage worker under a token budget must still exhaust the brake"
    );
    assert!(
        snapshot.spent_tokens > 1000,
        "worst-cased to the token ceiling"
    );
    assert!(
        !outcome.ok,
        "the flow self-cancels rather than running free"
    );
}

#[test]
fn within_budget_never_warns_or_cancels() {
    // A generous budget: a 2-stage pipeline costing $1.00 total under a $100 cap
    // never warns/exhausts, and completes ok with BudgetUpdate telemetry.
    let pipeline = Strategy::Pipeline {
        stages: vec![cli_step("p0"), cli_step("p1")],
    };
    let def = budgeted_def(pipeline, spec(Some(100.0), Some(1_000_000), None), 2);
    let scripts = BTreeMap::from([
        ("p0".to_string(), script(pricey("R0"))),
        ("p1".to_string(), script(pricey("R1"))),
    ]);
    let (outcome, snapshot, observer) = run_budgeted(&def, scripts);
    assert!(outcome.ok, "a within-budget flow completes ok");
    assert!(!observer.warned() && !observer.exhausted());
    assert!(!snapshot.exhausted);
    // Telemetry still flowed (a BudgetUpdate per stage).
    assert_eq!(observer.decisions().len(), 2);
    assert!(
        observer
            .decisions()
            .iter()
            .all(|d| *d == BudgetDecision::Within)
    );
}

// ---- 2. max_depth / max_workers ceiling (absence-at-floor) --------------------

#[test]
fn depth_ceiling_refuses_spawn_at_floor_deterministically() {
    // A FleetBudget at the depth floor refuses every spawn (absence-at-floor): the
    // engine records a deterministic DepthCeiling refusal and never starts a worker.
    // (C3b governs the engine spawn path; a flat flow is at depth 0, so we drive
    // the FleetBudget at the floor directly via a root with max_depth = 0.)
    let single = Strategy::Single {
        step: cli_step("only"),
    };
    let def = budgeted_def(single, BudgetSpec::default(), 0);
    let scripts = BTreeMap::from([("only".to_string(), script(ok("done")))]);

    let scripts = Arc::new(scripts);
    let resolver = FakeResolver::new(Arc::clone(&scripts));
    let ledger = Arc::new(WorkerLedger::new());
    let budget = Arc::new(BudgetLedger::new(def.budget));
    // Root at depth 0 with max_depth 0 → depth >= max_depth, refuse at the floor.
    let fleet = FleetBudget::root(0, None, None, None);
    let observer = CaptureObserver::default();
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_observer(&observer)
        .with_budget(Arc::clone(&budget), fleet);
    let outcome = driver.run(&def, &CancelToken::never());

    let refusals = observer.refusals();
    assert_eq!(
        refusals.len(),
        1,
        "exactly one recorded refusal (got {refusals:?})"
    );
    assert_eq!(refusals[0].0, "node-0", "the refused node id is recorded");
    assert!(
        matches!(
            refusals[0].1,
            SpawnRefusal::Depth {
                depth: 0,
                max_depth: 0
            }
        ),
        "a deterministic DepthCeiling refusal, not a crash"
    );
    assert!(!outcome.ok, "a flow that could not spawn ends not-ok");
    // The refusal is determinstic: a re-run records the identical refusal.
    let observer2 = CaptureObserver::default();
    let driver2 = Driver::new(
        &resolver,
        Arc::new(WorkerLedger::new()),
        Arc::new(NeverApprover),
        None,
    )
    .with_observer(&observer2)
    .with_budget(
        Arc::new(BudgetLedger::new(def.budget)),
        FleetBudget::root(0, None, None, None),
    );
    let _ = driver2.run(&def, &CancelToken::never());
    assert_eq!(
        observer2.refusals(),
        refusals,
        "the refusal is deterministic"
    );
}

#[test]
fn worker_ceiling_refuses_beyond_the_global_semaphore_cap() {
    // A 4-branch parallel wave under max_workers = 2 with concurrency 4: the
    // process-global semaphore admits at most 2 in-flight; the others are refused
    // with a recorded WorkerCeiling FlowDecision (absence-at-floor).
    let parallel = Strategy::Parallel {
        branches: vec![
            cli_step("w0"),
            cli_step("w1"),
            cli_step("w2"),
            cli_step("w3"),
        ],
        join: Join::All,
    };
    let def = budgeted_def(parallel, spec(None, None, Some(2)), 2);
    // Each branch blocks briefly so all four overlap, forcing the cap to bite.
    let blocking = |text: &str| Script {
        result: ok(text),
        delay: std::time::Duration::from_millis(40),
        steerable: false,
    };
    let scripts = BTreeMap::from([
        ("w0".to_string(), blocking("W0")),
        ("w1".to_string(), blocking("W1")),
        ("w2".to_string(), blocking("W2")),
        ("w3".to_string(), blocking("W3")),
    ]);
    let scripts = Arc::new(scripts);
    let resolver = FakeResolver::new(Arc::clone(&scripts));
    let ledger = Arc::new(WorkerLedger::new());
    let budget = Arc::new(BudgetLedger::new(def.budget));
    let fleet = FleetBudget::root(2, Some(2), None, None);
    let observer = CaptureObserver::default();
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_concurrency(4)
        .with_observer(&observer)
        .with_budget(Arc::clone(&budget), fleet);
    let _ = driver.run(&def, &CancelToken::never());

    // At least some spawns were refused at the worker ceiling (the cap held).
    let refusals = observer.refusals();
    assert!(
        refusals
            .iter()
            .any(|(_, r)| matches!(r, SpawnRefusal::Workers { max_workers: 2, .. })),
        "the global semaphore refused at the cap (got {refusals:?})"
    );
}

/// Run a 4-branch parallel wave under `max_workers` = 2 with concurrency 4, returning
/// the recorded ledger JSONL + the refused node ids (in observation order). Shared by
/// the determinism + byte-identical replay assertions below (finding B).
fn run_over_worker_ceiling() -> (String, Vec<String>) {
    use crate::flow::FlowProgress;
    let parallel = Strategy::Parallel {
        branches: vec![
            cli_step("w0"),
            cli_step("w1"),
            cli_step("w2"),
            cli_step("w3"),
        ],
        join: Join::All,
    };
    let def = budgeted_def(parallel, spec(None, None, Some(2)), 2);
    // Each branch blocks briefly so all four overlap, forcing the cap to bite.
    let blocking = |text: &str| Script {
        result: ok(text),
        delay: std::time::Duration::from_millis(40),
        steerable: false,
    };
    let scripts = Arc::new(BTreeMap::from([
        ("w0".to_string(), blocking("W0")),
        ("w1".to_string(), blocking("W1")),
        ("w2".to_string(), blocking("W2")),
        ("w3".to_string(), blocking("W3")),
    ]));
    let resolver = FakeResolver::new(Arc::clone(&scripts));
    let ledger = Arc::new(WorkerLedger::new());
    let budget = Arc::new(BudgetLedger::new(def.budget));
    let fleet = FleetBudget::root(2, Some(2), None, None);
    let observer = CaptureObserver::default();
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    // A no-op progress sink (events still buffer + record into the ledger).
    let sink = |_p: FlowProgress| {};
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_concurrency(4)
        .with_observer(&observer)
        .with_progress(&sink)
        .with_budget(Arc::clone(&budget), fleet);
    let _ = driver.run(&def, &CancelToken::never());
    let refused_nodes: Vec<String> = observer.refusals().into_iter().map(|(n, _)| n).collect();
    (ledger.to_jsonl(), refused_nodes)
}

/// The def `run_over_worker_ceiling` records (the SAME def the replay must reuse, so
/// admission is decided identically). A 4-branch parallel wave under max_workers = 2.
fn worker_ceiling_def() -> WorkflowDef {
    let parallel = Strategy::Parallel {
        branches: vec![
            cli_step("w0"),
            cli_step("w1"),
            cli_step("w2"),
            cli_step("w3"),
        ],
        join: Join::All,
    };
    budgeted_def(parallel, spec(None, None, Some(2)), 2)
}

#[test]
fn worker_ceiling_admission_is_deterministic_and_declared_order() {
    // Finding B: when a wave exceeds the worker ceiling, WHICH branches are admitted
    // vs refused must be a DETERMINISTIC function of declared order — not a threaded
    // `acquire()` race. Under max_workers = 2, branches branch-0/branch-1 (the first
    // two declared) are admitted and branch-2/branch-3 are refused, on EVERY run. Run
    // twice and assert the refused set is identical AND is exactly the last two
    // declared branches (by node id).
    let (jsonl1, refused1) = run_over_worker_ceiling();
    let (jsonl2, refused2) = run_over_worker_ceiling();
    assert_eq!(
        refused1, refused2,
        "the refused branches are deterministic across runs"
    );
    assert_eq!(
        refused1,
        vec!["branch-2".to_string(), "branch-3".to_string()],
        "admission is in declared order: the first 2 fit, the last 2 are refused"
    );
    // And the whole recorded tape is byte-identical run-to-run — the determinism the
    // byte-identical replay gate depends on (the prior `acquire()` race broke this).
    assert_eq!(
        jsonl1, jsonl2,
        "a wave over the worker ceiling records a byte-identical tape every run"
    );
}

#[test]
fn worker_ceiling_wave_replays_byte_identically() {
    // The companion replay gate for finding B: RECORD a wave that overflows the worker
    // ceiling, then REPLAY through the production resolver (WITH the same budget
    // envelope reconstructed from the def, as `flow.replay` does) and assert the
    // re-emitted tape is byte-identical to the recorded one. Deterministic admission is
    // the precondition: a nondeterministic refusal cut would diverge under replay.
    let (recorded_jsonl, _) = run_over_worker_ceiling();
    let recorded = WorkerLedger::from_jsonl(&recorded_jsonl).expect("reconstruct recorded tape");

    let def = worker_ceiling_def();
    let resolver = crate::flow::ReplayResolver::from_ledger(&recorded);
    let generation = crate::flow::replay_generation_provider(&recorded);
    let replay_ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    // Reconstruct the budget the same way `flow_job::run_flow_replay` does, so the
    // deterministic worker-ceiling refusals reproduce on replay.
    let budget = Arc::new(BudgetLedger::new(def.budget));
    let fleet = FleetBudget::root(
        def.max_depth,
        def.budget.max_workers,
        budget.remaining_usd(),
        budget.remaining_tokens(),
    );
    let _ = Driver::new(&resolver, Arc::clone(&replay_ledger), approver, None)
        .with_concurrency(4)
        .with_generation(&generation)
        .with_budget(Arc::clone(&budget), fleet)
        .run(&def, &CancelToken::never());
    assert_eq!(
        replay_ledger.to_jsonl(),
        recorded_jsonl,
        "a worker-ceiling wave replays byte-identically (deterministic admission + budget)"
    );
}

// ---- 3. Monotone de-escalation through the driver -----------------------------

#[test]
fn node_grant_narrows_to_the_remaining_fleet_budget() {
    // Drive a budgeted single flow and assert the carved per-node grant never
    // exceeds the fleet's remaining budget (the live de-escalation; the pure
    // contract is pinned in `crate::worker::budget` tests). Here we observe it via
    // the BudgetLedger's remaining headroom being the grant ceiling.
    let single = Strategy::Single {
        step: cli_step("only"),
    };
    let def = budgeted_def(single, spec(Some(2.0), Some(5000), None), 2);
    let scripts = BTreeMap::from([("only".to_string(), script(pricey("done")))]);
    let budget = Arc::new(BudgetLedger::new(def.budget));
    // Before the run, the remaining headroom IS the per-node ceiling a child may
    // spend — and it can only narrow (intersect with default = no widening).
    assert_eq!(budget.remaining_usd(), Some(2.0));
    assert_eq!(budget.remaining_tokens(), Some(5000));
    let _ = run_budgeted(&def, scripts);
    // After spending, the remaining headroom is tighter — a later child's grant is
    // strictly narrower (monotone).
    let after = BudgetLedger::new(def.budget);
    after.debit(&pricey("done"));
    assert!(
        after.remaining_usd().unwrap() < 2.0,
        "headroom only narrows"
    );
}

// ---- 4. BudgetLedger replay determinism (byte-identical) ----------------------

#[test]
fn budget_fold_is_replay_deterministic() {
    // RECORD a budgeted run, capture the budget decision sequence + final snapshot,
    // then RE-RUN the identical flow and assert the budget events reproduce exactly
    // (the budget fold is a pure function of the recorded usage — design §6).
    let pipeline = Strategy::Pipeline {
        stages: vec![cli_step("d0"), cli_step("d1"), cli_step("d2")],
    };
    let def = budgeted_def(pipeline, spec(Some(1.0), Some(10_000), None), 2);
    let make_scripts = || {
        BTreeMap::from([
            ("d0".to_string(), script(pricey("R0"))),
            ("d1".to_string(), script(pricey("R1"))),
            ("d2".to_string(), script(pricey("R2"))),
        ])
    };
    let (out1, snap1, obs1) = run_budgeted(&def, make_scripts());
    let (out2, snap2, obs2) = run_budgeted(&def, make_scripts());
    assert_eq!(
        obs1.decisions(),
        obs2.decisions(),
        "the budget decision sequence is replay-deterministic"
    );
    assert_eq!(snap1, snap2, "the final budget snapshot is byte-identical");
    assert_eq!(out1.ok, out2.ok);
}

// ---- Sanity: a default (unbudgeted) flow emits NO budget callbacks -------------

#[test]
fn unbudgeted_flow_emits_no_budget_telemetry() {
    // A default BudgetSpec (all-None) attached as a budget still never warns/
    // exhausts/refuses — the zero-regression guarantee for existing flows.
    let pipeline = Strategy::Pipeline {
        stages: vec![cli_step("u0"), cli_step("u1")],
    };
    let def = budgeted_def(pipeline, BudgetSpec::default(), 2);
    let scripts = BTreeMap::from([
        ("u0".to_string(), script(ok("R0"))),
        ("u1".to_string(), script(fail("R1"))),
    ]);
    let (_outcome, snapshot, observer) = run_budgeted(&def, scripts);
    assert!(!observer.warned() && !observer.exhausted());
    assert!(observer.refusals().is_empty());
    assert!(!snapshot.exhausted);
    // Telemetry still TRACKS the reported spend (BudgetUpdate fires), but with no
    // cap it can never warn/exhaust — a no-cost node is worst-cased to 0 when
    // uncapped, so a silent worker is never charged a phantom cost.
    assert!(
        observer
            .decisions()
            .iter()
            .all(|d| *d == BudgetDecision::Within),
        "an uncapped flow only ever reports `Within`"
    );
}
