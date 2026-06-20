//! RECORD / REPLAY / GOLDEN tests for the deterministic engine (design §3).
//!
//! These mirror the kernel's golden-test discipline one layer up: the engine is
//! pure, workers are the only nondeterminism, and the [`WorkerLedger`] tape makes
//! a run reproducible. Three modes are exercised across this module and
//! [`replay`]:
//!
//! - **GOLDEN** (here) — drive [`FakeWorker`]s (scripted [`TurnResult`]s + canned
//!   events) through the [`Driver`] and snapshot the aggregated [`FlowOutcome`]
//!   with `insta`, including a `Parallel` whose branches finish OUT OF ORDER
//!   (proving the declared-order fold), plus `FirstOk` and `Quorum`
//!   (reached + short/tie).
//! - **REPLAY** ([`replay`]) — RECORD a `FakeWorker` run, then REPLAY from the
//!   recorded ledger with a [`ReplayWorker`] and assert byte-identical engine
//!   output + final tape.
//! - **CONTRACT** ([`replay`]) — the declared-order-fold invariant pinned.
//!
//! The shared scripted-worker substrate (the harness both modes use) lives here
//! so the snapshot module path stays `flow::tests` (stable snapshot filenames);
//! this whole directory is under `/tests/`, so it is excluded from the file-size
//! gate (pure test code).

mod budget;
mod replay;

use super::{Driver, FlowOutcome, WorkerResolver};
use crate::delegate_proxy::DelegateApprover;
use crate::worker::{
    AgentWorker, LedgerPayload, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerKind,
    WorkerLedger, WorkerSession, WorkerTask, synthesize_turn_steps,
};
use nerve_core::CancelToken;
use nerve_runtime::{
    BudgetSpec, FailPolicy, Join, RiskTier, SessionApprovalDecision, Step, Strategy, TaskTemplate,
    WorkerRef, WorkflowDef,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- Scripted worker substrate ------------------------------------------------

/// One node's script: the final [`TurnResult`], an optional pre-result delay (to
/// force out-of-order completion in a parallel wave), and whether the session is
/// steerable (so a `flow.steer` test can drive a follow-up turn). Keyed by the
/// rendered prompt, which is unique per node in these tests.
#[derive(Clone)]
struct Script {
    result: TurnResult,
    delay: Duration,
    steerable: bool,
}

fn ok(text: &str) -> TurnResult {
    TurnResult {
        ok: true,
        text: text.into(),
        usage: nerve_agent::Usage {
            input_tokens: 5,
            output_tokens: 3,
            ..nerve_agent::Usage::default()
        },
        cost_usd: Some(0.001),
        timed_out: false,
    }
}

fn fail(text: &str) -> TurnResult {
    TurnResult {
        ok: false,
        text: text.into(),
        usage: nerve_agent::Usage::default(),
        cost_usd: None,
        timed_out: false,
    }
}

/// A worker that emits the canonical synthesized step stream for its scripted
/// result, then returns it — no LLM, no subprocess. Keyed by the rendered prompt.
struct FakeWorker {
    scripts: Arc<BTreeMap<String, Script>>,
    provider: bool,
    captured: Arc<Mutex<Vec<String>>>,
}

impl AgentWorker for FakeWorker {
    fn kind(&self) -> WorkerKind {
        if self.provider {
            WorkerKind::Provider {
                provider: "fake".into(),
                model: "fake".into(),
            }
        } else {
            WorkerKind::Cli("claude")
        }
    }

    fn capability(&self) -> RiskTier {
        RiskTier::ReadOnly
    }

    fn start(
        &self,
        task: &WorkerTask,
        _ctx: &WorkerContext,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        // Record the rendered prompt this worker actually received (so a test can
        // assert the engine interpolated upstream outputs into it).
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(task.prompt.clone());
        let script = self
            .scripts
            .get(&task.prompt)
            .cloned()
            .unwrap_or_else(|| Script {
                result: fail(&format!("no script for prompt `{}`", task.prompt)),
                delay: Duration::ZERO,
                steerable: false,
            });
        if !script.delay.is_zero() {
            std::thread::sleep(script.delay);
        }
        synthesize_turn_steps(1, &script.result, on_event);
        Ok(Box::new(ScriptedSession {
            last: script.result,
            steerable: script.steerable,
        }))
    }
}

/// A scripted session: turn 1 already ran in `start`. A `steerable` session runs a
/// follow-up turn on [`Self::steer`] that synthesizes a turn echoing the steer
/// message; a non-steerable one returns [`WorkerError::NotSteerable`] (modeling a
/// one-shot worker like `gemini`).
struct ScriptedSession {
    last: TurnResult,
    steerable: bool,
}

impl WorkerSession for ScriptedSession {
    fn steer(
        &mut self,
        message: &str,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        if !self.steerable {
            return Err(WorkerError::NotSteerable);
        }
        // The follow-up turn echoes the steer message so a test can assert the
        // worker received it; it becomes this session's new last result.
        let turn = ok(&format!("steered: {message}"));
        synthesize_turn_steps(2, &turn, on_event);
        self.last = turn.clone();
        Ok(turn)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

/// A resolver that hands every node a [`FakeWorker`] over the shared scripts. CLI
/// refs get a CLI-kind fake; provider refs a provider-kind fake. An optional
/// capture sink records the rendered prompt each worker actually received (in
/// `start()` call order — sequential for a pipeline), so a test can assert the
/// engine interpolated upstream outputs into a downstream stage's task.
struct FakeResolver {
    scripts: Arc<BTreeMap<String, Script>>,
    captured: Arc<Mutex<Vec<String>>>,
}

impl FakeResolver {
    fn new(scripts: Arc<BTreeMap<String, Script>>) -> Self {
        Self {
            scripts,
            captured: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl WorkerResolver for FakeResolver {
    fn resolve(&self, worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let provider = matches!(worker_ref, WorkerRef::Provider { .. });
        Ok(Box::new(FakeWorker {
            scripts: Arc::clone(&self.scripts),
            provider,
            captured: Arc::clone(&self.captured),
        }))
    }
}

// ---- Replay worker substrate --------------------------------------------------

/// A worker that re-emits a RECORDED node's events instead of calling an
/// LLM/subprocess (design §3, REPLAY). It is keyed by the rendered prompt → node
/// id captured at RECORD time, so each replayed node re-emits exactly its own
/// recorded events and returns its recorded result.
struct ReplayWorker {
    /// The recorded tape (immutable; shared across all replay workers).
    recorded: Arc<Vec<crate::worker::LedgerEntry>>,
    /// prompt -> node_id, captured during the recorded run.
    prompt_to_node: Arc<BTreeMap<String, String>>,
}

impl AgentWorker for ReplayWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Cli("claude")
    }
    fn capability(&self) -> RiskTier {
        RiskTier::ReadOnly
    }

    fn start(
        &self,
        task: &WorkerTask,
        _ctx: &WorkerContext,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let node = self
            .prompt_to_node
            .get(&task.prompt)
            .cloned()
            .ok_or_else(|| WorkerError::Start(format!("no recorded node for `{}`", task.prompt)))?;
        // Re-emit this node's recorded events, in recorded seq order, and recover
        // its recorded final result — never touching an LLM/process.
        let mut last = fail("replay: node had no recorded result");
        for entry in self.recorded.iter().filter(|e| e.node_id == node) {
            match &entry.payload {
                LedgerPayload::Event(event) => on_event(event.clone()),
                LedgerPayload::Result(result) => last = result.clone(),
            }
        }
        Ok(Box::new(ScriptedSession {
            last,
            steerable: false,
        }))
    }
}

/// A resolver handing out [`ReplayWorker`]s over a recorded tape.
struct ReplayResolver {
    recorded: Arc<Vec<crate::worker::LedgerEntry>>,
    prompt_to_node: Arc<BTreeMap<String, String>>,
}

impl WorkerResolver for ReplayResolver {
    fn resolve(&self, _worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        Ok(Box::new(ReplayWorker {
            recorded: Arc::clone(&self.recorded),
            prompt_to_node: Arc::clone(&self.prompt_to_node),
        }))
    }
}

// ---- Shared harness -----------------------------------------------------------

/// A deny-all approver (the scripted workers never ask, so it is never consulted).
struct NeverApprover;
impl DelegateApprover for NeverApprover {
    fn request(
        &self,
        _session_id: &str,
        _tool: &str,
        _args: &Value,
        _tier: RiskTier,
        _preview: String,
        _cancel: &CancelToken,
    ) -> SessionApprovalDecision {
        SessionApprovalDecision::Deny
    }
}

fn cli_step(prompt: &str) -> Step {
    Step {
        worker: WorkerRef::Cli {
            name: "claude".into(),
        },
        task: TaskTemplate::new(prompt),
        autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
        on_fail: FailPolicy::Continue,
    }
}

/// A CLI step that aborts the flow on failure (the pipeline default semantics).
fn pipeline_step(prompt: &str) -> Step {
    Step {
        worker: WorkerRef::Cli {
            name: "claude".into(),
        },
        task: TaskTemplate::new(prompt),
        autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
        on_fail: FailPolicy::Abort,
    }
}

fn def(name: &str, strategy: Strategy) -> WorkflowDef {
    WorkflowDef {
        schema_version: 1,
        name: name.into(),
        strategy,
        budget: BudgetSpec::default(),
        max_depth: 2,
    }
}

/// Run `def` through the engine over `scripts`, returning the outcome AND the
/// recorded ledger (the RECORD step). Concurrency is pinned high enough that all
/// branches overlap, so delays genuinely reorder completion.
fn record(
    def: &WorkflowDef,
    scripts: BTreeMap<String, Script>,
) -> (FlowOutcome, Arc<WorkerLedger>) {
    let (outcome, ledger, _captured) = record_capturing(def, scripts);
    (outcome, ledger)
}

/// Like [`record`], but also returns the rendered prompts each worker received (in
/// `start()` call order). For a `Pipeline` (sequential) that is stage order, so a
/// test can assert a downstream stage's task interpolated upstream outputs.
fn record_capturing(
    def: &WorkflowDef,
    scripts: BTreeMap<String, Script>,
) -> (FlowOutcome, Arc<WorkerLedger>, Vec<String>) {
    let scripts = Arc::new(scripts);
    let resolver = FakeResolver::new(Arc::clone(&scripts));
    let captured = Arc::clone(&resolver.captured);
    let ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None).with_concurrency(8);
    let outcome = driver.run(def, &CancelToken::never());
    let captured = crate::sync::lock_recover(&captured).clone();
    (outcome, ledger, captured)
}

/// Build the prompt -> node_id map from a recorded tape (each node's first entry
/// names it; the prompt is recovered from the def in declared order).
fn prompt_to_node(
    def: &WorkflowDef,
    recorded: &[crate::worker::LedgerEntry],
) -> BTreeMap<String, String> {
    let prompts = declared_prompts(def);
    let mut node_ids: Vec<String> = Vec::new();
    for entry in recorded {
        if !node_ids.contains(&entry.node_id) {
            node_ids.push(entry.node_id.clone());
        }
    }
    node_ids.sort(); // deterministic: branch-0, branch-1, ... or node-0
    let mut map = BTreeMap::new();
    for (prompt, node) in prompts.into_iter().zip(node_ids) {
        map.insert(prompt, node);
    }
    map
}

/// The declared prompts in step order (so each maps to its deterministic node id).
/// For a `Pipeline` these are the RAW (un-interpolated) stage prompts, so the
/// replay fixture uses non-interpolated stage prompts (declared == rendered); the
/// interpolation property is pinned separately by a prompt-capturing test.
fn declared_prompts(def: &WorkflowDef) -> Vec<String> {
    match &def.strategy {
        Strategy::Single { step } => vec![step.task.prompt.clone()],
        Strategy::Parallel { branches, .. } => {
            branches.iter().map(|s| s.task.prompt.clone()).collect()
        }
        Strategy::Pipeline { stages } => stages.iter().map(|s| s.task.prompt.clone()).collect(),
        _ => Vec::new(),
    }
}

/// A compact, golden-friendly rendering of an outcome (ok + summary + the kept
/// results' text, in order).
fn render_outcome(outcome: &FlowOutcome) -> String {
    let mut out = format!("ok={}\nsummary={}\nresults:\n", outcome.ok, outcome.summary);
    for (i, result) in outcome.results.iter().enumerate() {
        out.push_str(&format!(
            "  [{i}] ok={} text={:?}\n",
            result.ok, result.text
        ));
    }
    out
}

/// A 3-branch `Parallel` where branch 0 sleeps longest and branch 2 returns
/// first; the fold MUST still be in declared (branch index) order. Shared by the
/// golden + replay tests as the canonical out-of-order fixture.
fn parallel_out_of_order(join: Join) -> (WorkflowDef, BTreeMap<String, Script>) {
    let workflow = def(
        "parallel",
        Strategy::Parallel {
            branches: vec![cli_step("task A"), cli_step("task B"), cli_step("task C")],
            join,
        },
    );
    let scripts = BTreeMap::from([
        (
            "task A".to_string(),
            Script {
                result: ok("answer A"),
                delay: Duration::from_millis(60), // finishes LAST
                steerable: false,
            },
        ),
        (
            "task B".to_string(),
            Script {
                result: ok("answer B"),
                delay: Duration::from_millis(30),
                steerable: false,
            },
        ),
        (
            "task C".to_string(),
            Script {
                result: ok("answer C"),
                delay: Duration::ZERO, // finishes FIRST
                steerable: false,
            },
        ),
    ]);
    (workflow, scripts)
}

// ---- GOLDEN: Single -----------------------------------------------------------

#[test]
fn golden_single() {
    let workflow = def(
        "single",
        Strategy::Single {
            step: cli_step("the only task"),
        },
    );
    let scripts = BTreeMap::from([(
        "the only task".to_string(),
        Script {
            result: ok("the single answer"),
            delay: Duration::ZERO,
            steerable: false,
        },
    )]);
    let (outcome, _) = record(&workflow, scripts);
    insta::assert_snapshot!("golden_single", render_outcome(&outcome));
}

// ---- GOLDEN: Parallel with OUT-OF-ORDER completion → declared-order fold -------

#[test]
fn golden_parallel_all_declared_order_despite_completion_order() {
    let (workflow, scripts) = parallel_out_of_order(Join::All);
    let (outcome, _) = record(&workflow, scripts);
    // Despite C finishing first and A last, the fold is A, B, C (declared order).
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>(),
        vec!["answer A", "answer B", "answer C"],
        "fold must be in declared order, not completion order"
    );
    insta::assert_snapshot!("golden_parallel_all", render_outcome(&outcome));
}

#[test]
fn golden_parallel_first_ok_picks_first_declared_ok() {
    // Branch A fails; B and C succeed. FirstOk must pick B (first OK in declared
    // order), NOT C (which finishes first).
    let workflow = def(
        "first_ok",
        Strategy::Parallel {
            branches: vec![cli_step("task A"), cli_step("task B"), cli_step("task C")],
            join: Join::FirstOk,
        },
    );
    let scripts = BTreeMap::from([
        (
            "task A".to_string(),
            Script {
                result: fail("A failed"),
                delay: Duration::ZERO,
                steerable: false,
            },
        ),
        (
            "task B".to_string(),
            Script {
                result: ok("answer B"),
                delay: Duration::from_millis(40), // finishes after C
                steerable: false,
            },
        ),
        (
            "task C".to_string(),
            Script {
                result: ok("answer C"),
                delay: Duration::ZERO, // finishes first, but is later in order
                steerable: false,
            },
        ),
    ]);
    let (outcome, _) = record(&workflow, scripts);
    assert_eq!(outcome.results.len(), 1);
    assert_eq!(outcome.results[0].text, "answer B");
    insta::assert_snapshot!("golden_parallel_first_ok", render_outcome(&outcome));
}

#[test]
fn golden_parallel_quorum_reached() {
    // n=2 with three OK branches → quorum reached, keep first 2 in declared order.
    let (workflow, scripts) = parallel_out_of_order(Join::Quorum { n: 2 });
    let (outcome, _) = record(&workflow, scripts);
    assert!(outcome.ok);
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>(),
        vec!["answer A", "answer B"],
        "quorum keeps the first n OKs in declared order"
    );
    insta::assert_snapshot!("golden_parallel_quorum_reached", render_outcome(&outcome));
}

#[test]
fn golden_parallel_quorum_short() {
    // n=3 but only 1 branch succeeds → quorum SHORT (not ok), keeps what oks exist.
    let workflow = def(
        "quorum_short",
        Strategy::Parallel {
            branches: vec![cli_step("task A"), cli_step("task B"), cli_step("task C")],
            join: Join::Quorum { n: 3 },
        },
    );
    let scripts = BTreeMap::from([
        (
            "task A".to_string(),
            Script {
                result: fail("A failed"),
                delay: Duration::ZERO,
                steerable: false,
            },
        ),
        (
            "task B".to_string(),
            Script {
                result: ok("answer B"),
                delay: Duration::ZERO,
                steerable: false,
            },
        ),
        (
            "task C".to_string(),
            Script {
                result: fail("C failed"),
                delay: Duration::ZERO,
                steerable: false,
            },
        ),
    ]);
    let (outcome, _) = record(&workflow, scripts);
    assert!(!outcome.ok, "a short quorum is not ok");
    insta::assert_snapshot!("golden_parallel_quorum_short", render_outcome(&outcome));
}

// ---- Script helper + Pipeline GOLDEN / interpolation / replay ------------------

/// A scripted result for `prompt` (no delay, not steerable) — the common case.
fn script(result: TurnResult) -> Script {
    Script {
        result,
        delay: Duration::ZERO,
        steerable: false,
    }
}

/// A 3-stage pipeline where stage 1 interpolates stage 0's output via `{{prev}}`
/// and stage 2 interpolates stage 0 by node id via `{{stage-0}}`. The scripts are
/// keyed by the RENDERED (interpolated) prompts the stages will produce, so a miss
/// would surface as a `no script` failure.
fn interpolating_pipeline() -> (WorkflowDef, BTreeMap<String, Script>) {
    let workflow = def(
        "pipe",
        Strategy::Pipeline {
            stages: vec![
                pipeline_step("draft the spec"),
                pipeline_step("review: {{prev}}"),
                pipeline_step("ship using stage0={{stage-0}}"),
            ],
        },
    );
    let scripts = BTreeMap::from([
        ("draft the spec".to_string(), script(ok("SPEC"))),
        // Stage 1 sees stage 0's output via {{prev}} → "review: SPEC".
        ("review: SPEC".to_string(), script(ok("REVIEWED"))),
        // Stage 2 sees stage 0's output via {{stage-0}} → "ship using stage0=SPEC".
        ("ship using stage0=SPEC".to_string(), script(ok("SHIPPED"))),
    ]);
    (workflow, scripts)
}

#[test]
fn golden_pipeline_interpolates_upstream_outputs_into_downstream_stages() {
    let (workflow, scripts) = interpolating_pipeline();
    let (outcome, _ledger, captured) = record_capturing(&workflow, scripts);
    // The downstream stages RECEIVED the interpolated prompts (the core property:
    // the cross-stage blackboard fed stage N from stages < N).
    assert_eq!(
        captured,
        vec![
            "draft the spec".to_string(),
            "review: SPEC".to_string(),
            "ship using stage0=SPEC".to_string(),
        ],
        "each stage's worker received its interpolated task in stage order"
    );
    // The pipeline's answer is the last stage; all stages are kept in order.
    assert!(outcome.ok);
    assert_eq!(
        outcome
            .results
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>(),
        vec!["SPEC", "REVIEWED", "SHIPPED"],
    );
    insta::assert_snapshot!("golden_pipeline", render_outcome(&outcome));
}

#[test]
fn pipeline_runs_stages_in_declared_order_independent_of_scripts() {
    // Determinism: the stage order is the DECLARED order regardless of result
    // contents. Running the same pipeline twice yields a byte-identical outcome.
    let (workflow, scripts) = interpolating_pipeline();
    let (first, _, _) = record_capturing(&workflow, scripts);
    let (_, scripts2) = interpolating_pipeline();
    let (second, _, captured2) = record_capturing(&workflow, scripts2);
    assert_eq!(render_outcome(&first), render_outcome(&second));
    assert_eq!(
        captured2.first().map(String::as_str),
        Some("draft the spec")
    );
}

#[test]
fn replay_pipeline_is_byte_identical() {
    // RECORD a pipeline (non-interpolated stage prompts so declared == rendered),
    // then REPLAY from the recorded ledger and assert byte-identical tape + outcome.
    let workflow = def(
        "pipe",
        Strategy::Pipeline {
            stages: vec![
                pipeline_step("alpha"),
                pipeline_step("beta"),
                pipeline_step("gamma"),
            ],
        },
    );
    let scripts = BTreeMap::from([
        ("alpha".to_string(), script(ok("A"))),
        ("beta".to_string(), script(ok("B"))),
        ("gamma".to_string(), script(ok("C"))),
    ]);
    let (recorded_outcome, recorded_ledger) = record(&workflow, scripts);
    let recorded_jsonl = recorded_ledger.to_jsonl();
    let recorded_tape = Arc::new(recorded_ledger.snapshot());

    let map = Arc::new(prompt_to_node(&workflow, &recorded_tape));
    let resolver = ReplayResolver {
        recorded: Arc::clone(&recorded_tape),
        prompt_to_node: Arc::clone(&map),
    };
    let replay_ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&replay_ledger), approver, None);
    let replay_outcome = driver.run(&workflow, &CancelToken::never());

    assert_eq!(
        render_outcome(&replay_outcome),
        render_outcome(&recorded_outcome),
        "pipeline replay must reproduce the recorded outcome exactly"
    );
    assert_eq!(
        replay_ledger.to_jsonl(),
        recorded_jsonl,
        "replayed pipeline ledger must be byte-identical to the recorded ledger"
    );
}

// ---- flow.steer (the live-flow worker registry) -------------------------------

/// Register a steerable scripted session under `node` in a fresh registry, having
/// already "run" turn 1 (so it is a live frontier ready for a follow-up).
fn live_frontier(node: &str, turn1: TurnResult, steerable: bool) -> crate::worker::SteerRegistry {
    let registry = crate::worker::SteerRegistry::new();
    registry.register(
        node,
        Box::new(ScriptedSession {
            last: turn1,
            steerable,
        }),
    );
    registry
}

#[test]
fn steer_runs_a_followup_turn_on_the_live_frontier() {
    let registry = live_frontier("stage-0", ok("turn 1"), true);
    let ledger = WorkerLedger::new();
    let mut seen: Vec<(String, WorkerEvent)> = Vec::new();
    let (node, result) = registry
        .steer(
            None, // the only live worker
            "now fix the lint",
            &CancelToken::never(),
            &ledger,
            &mut |node, event| seen.push((node.to_string(), event)),
        )
        .expect("steer the only live worker");
    assert_eq!(node, "stage-0");
    // The worker RECEIVED the steer message (its follow-up echoes it).
    assert_eq!(result.text, "steered: now fix the lint");
    // A follow-up turn streamed node-scoped events (turn 2) and was recorded.
    assert!(seen.iter().all(|(n, _)| n == "stage-0"));
    assert!(
        seen.iter().any(|(_, e)| matches!(
            e,
            WorkerEvent::Step(nerve_runtime::AgentEventKind::Message { text }) if text == "steered: now fix the lint"
        )),
        "the follow-up turn emitted the steered message as a FlowNodeAgent step"
    );
    // The steered turn is on the replay tape (recorded nondeterminism, §5).
    assert_eq!(
        ledger.output("stage-0"),
        Some("steered: now fix the lint".into())
    );
}

#[test]
fn steer_targets_a_specific_node_by_id() {
    let registry = live_frontier("node-0", ok("t1"), true);
    let ledger = WorkerLedger::new();
    let (node, _) = registry
        .steer(
            Some("node-0"),
            "go",
            &CancelToken::never(),
            &ledger,
            &mut |_, _| {},
        )
        .expect("steer node-0 by id");
    assert_eq!(node, "node-0");
}

#[test]
fn steer_errors_on_a_non_steerable_frontier() {
    // A one-shot worker (gemini-like) returns NotSteerable.
    let registry = live_frontier("stage-0", ok("t1"), false);
    let ledger = WorkerLedger::new();
    let err = registry
        .steer(None, "go", &CancelToken::never(), &ledger, &mut |_, _| {})
        .expect_err("a one-shot worker is not steerable");
    assert!(matches!(err, crate::worker::SteerError::NotSteerable));
}

#[test]
fn steer_errors_when_no_live_branch_matches() {
    let registry = crate::worker::SteerRegistry::new();
    let ledger = WorkerLedger::new();
    // No live worker at all.
    let err = registry
        .steer(None, "go", &CancelToken::never(), &ledger, &mut |_, _| {})
        .expect_err("no live worker");
    assert!(matches!(err, crate::worker::SteerError::NoLiveBranch(_)));
    // A named node that is not live.
    let registry = live_frontier("stage-0", ok("t1"), true);
    let err = registry
        .steer(
            Some("stage-9"),
            "go",
            &CancelToken::never(),
            &ledger,
            &mut |_, _| {},
        )
        .expect_err("unknown node");
    assert!(matches!(err, crate::worker::SteerError::NoLiveBranch(_)));
}

#[test]
fn steer_with_unset_selector_is_ambiguous_when_multiple_are_live() {
    let registry = crate::worker::SteerRegistry::new();
    registry.register(
        "branch-0",
        Box::new(ScriptedSession {
            last: ok("a"),
            steerable: true,
        }),
    );
    registry.register(
        "branch-1",
        Box::new(ScriptedSession {
            last: ok("b"),
            steerable: true,
        }),
    );
    let ledger = WorkerLedger::new();
    let err = registry
        .steer(None, "go", &CancelToken::never(), &ledger, &mut |_, _| {})
        .expect_err("ambiguous unset selector");
    assert!(matches!(err, crate::worker::SteerError::Ambiguous(2)));
}

#[test]
fn steer_after_close_errors() {
    let registry = live_frontier("stage-0", ok("t1"), true);
    registry.close("stage-0");
    let ledger = WorkerLedger::new();
    let err = registry
        .steer(
            Some("stage-0"),
            "go",
            &CancelToken::never(),
            &ledger,
            &mut |_, _| {},
        )
        .expect_err("a closed frontier is no longer steerable");
    assert!(matches!(err, crate::worker::SteerError::NoLiveBranch(_)));
}
