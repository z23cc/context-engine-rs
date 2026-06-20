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
use std::sync::Arc;
use std::time::Duration;

// ---- Scripted worker substrate ------------------------------------------------

/// One node's script: the final [`TurnResult`] and an optional pre-result delay
/// (to force out-of-order completion in a parallel wave). Keyed by the rendered
/// prompt, which is unique per node in these tests.
#[derive(Clone)]
struct Script {
    result: TurnResult,
    delay: Duration,
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
        let script = self
            .scripts
            .get(&task.prompt)
            .cloned()
            .unwrap_or_else(|| Script {
                result: fail(&format!("no script for prompt `{}`", task.prompt)),
                delay: Duration::ZERO,
            });
        if !script.delay.is_zero() {
            std::thread::sleep(script.delay);
        }
        synthesize_turn_steps(1, &script.result, on_event);
        Ok(Box::new(ScriptedSession {
            last: script.result,
        }))
    }
}

/// A one-shot scripted session (turn 1 already ran in `start`).
struct ScriptedSession {
    last: TurnResult,
}

impl WorkerSession for ScriptedSession {
    fn steer(
        &mut self,
        _message: &str,
        _cancel: &CancelToken,
        _on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        Err(WorkerError::NotSteerable)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

/// A resolver that hands every node a [`FakeWorker`] over the shared scripts. CLI
/// refs get a CLI-kind fake; provider refs a provider-kind fake.
struct FakeResolver {
    scripts: Arc<BTreeMap<String, Script>>,
}

impl WorkerResolver for FakeResolver {
    fn resolve(&self, worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let provider = matches!(worker_ref, WorkerRef::Provider { .. });
        Ok(Box::new(FakeWorker {
            scripts: Arc::clone(&self.scripts),
            provider,
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
        Ok(Box::new(ScriptedSession { last }))
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
    let scripts = Arc::new(scripts);
    let resolver = FakeResolver {
        scripts: Arc::clone(&scripts),
    };
    let ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&ledger), approver, None).with_concurrency(8);
    let outcome = driver.run(def, &CancelToken::never());
    (outcome, ledger)
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
fn declared_prompts(def: &WorkflowDef) -> Vec<String> {
    match &def.strategy {
        Strategy::Single { step } => vec![step.task.prompt.clone()],
        Strategy::Parallel { branches, .. } => {
            branches.iter().map(|s| s.task.prompt.clone()).collect()
        }
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
            },
        ),
        (
            "task B".to_string(),
            Script {
                result: ok("answer B"),
                delay: Duration::from_millis(30),
            },
        ),
        (
            "task C".to_string(),
            Script {
                result: ok("answer C"),
                delay: Duration::ZERO, // finishes FIRST
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
            },
        ),
        (
            "task B".to_string(),
            Script {
                result: ok("answer B"),
                delay: Duration::from_millis(40), // finishes after C
            },
        ),
        (
            "task C".to_string(),
            Script {
                result: ok("answer C"),
                delay: Duration::ZERO, // finishes first, but is later in order
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
            },
        ),
        (
            "task B".to_string(),
            Script {
                result: ok("answer B"),
                delay: Duration::ZERO,
            },
        ),
        (
            "task C".to_string(),
            Script {
                result: fail("C failed"),
                delay: Duration::ZERO,
            },
        ),
    ]);
    let (outcome, _) = record(&workflow, scripts);
    assert!(!outcome.ok, "a short quorum is not ok");
    insta::assert_snapshot!("golden_parallel_quorum_short", render_outcome(&outcome));
}
