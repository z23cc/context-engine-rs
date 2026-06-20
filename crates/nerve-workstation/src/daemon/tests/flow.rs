//! C2: daemon-level integration tests for the `flow.*` command family.
//!
//! These drive the additive `flow.*` protocol through the real router / job /
//! flow-engine plumbing with a **fake `claude`** (a tiny `/bin/sh` script that
//! speaks the stream-json protocol), so the deterministic C1 engine runs over real
//! contained subprocesses without a live LLM. A `flow.start` of a two-branch
//! `Parallel` of CLI workers exercises the full Flow* event sequence
//! (`flow_started` → `flow_node_started`×N → `flow_node_agent`* →
//! `flow_node_finished`×N → `flow_completed`) plus `flow.get` / `flow.list` /
//! `flow.close`.
//!
//! Hermetic: no network, the fake claude replies to one message and exits on EOF,
//! and CLI flow workers require the `--allow-delegate` lift (provider workers would
//! not) — mirroring the C1 FlowArgs gating.

use super::super::router::RuntimeDaemonRouter;
use super::{
    Arc, Mutex, Value, dispatch, json, response_with_id, rpc, runtime_with_file,
    wait_for_job_event, wait_for_job_terminal,
};
use crate::providers::ProviderRegistry;
use std::os::unix::fs::PermissionsExt as _;

/// A one-shot fake claude: emit an init line, then for the single stream-json user
/// line read from stdin, echo an assistant line + a result line and exit on EOF.
/// A flow CLI worker runs turn 1 only (`AgentWorker::start`), so one reply suffices.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '{"type":"system","subtype":"init","session_id":"flow-fake-1"}\n'
while IFS= read -r line; do
  msg=$(printf '%s' "$line" | sed 's/.*"text":"\([^"]*\)".*/\1/')
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"did %s"}]}}\n' "$msg"
  printf '{"type":"result","subtype":"success","is_error":false,"result":"result for %s","session_id":"flow-fake-1","num_turns":1,"total_cost_usd":0.001,"usage":{"input_tokens":5,"output_tokens":3}}\n' "$msg"
done
"#;

/// A launcher whose persistent path spawns the fake-claude script (ignoring the
/// requested program), so the flow's `CliWorker` drives a real contained child that
/// speaks the protocol.
struct FakeClaudeLauncher {
    _dir: tempfile::TempDir,
    script: std::path::PathBuf,
}

impl FakeClaudeLauncher {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-claude.sh");
        std::fs::write(&script, FAKE_CLAUDE).expect("write fake claude");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");
        Arc::new(Self { _dir: dir, script })
    }
}

impl crate::sandbox::SandboxLauncher for FakeClaudeLauncher {
    fn launch(
        &self,
        _spec: &crate::sandbox::CommandSpec,
        _policy: &crate::sandbox::SandboxPolicy,
        _cancel: &nerve_core::CancelToken,
    ) -> anyhow::Result<crate::sandbox::Output> {
        anyhow::bail!("fake claude launcher only supports the persistent path")
    }

    fn launch_persistent(
        &self,
        _spec: &crate::sandbox::CommandSpec,
        policy: &crate::sandbox::SandboxPolicy,
    ) -> anyhow::Result<crate::sandbox::PersistentChild> {
        let spec = crate::sandbox::CommandSpec {
            command: self.script.display().to_string(),
            args: Vec::new(),
        };
        crate::sandbox::PersistentChild::spawn(&spec, policy)
    }
}

/// A router whose flow CLI workers are LIFTED (`allow_delegate = true`) and whose
/// delegate launcher is the fake claude, so a `flow.start` of CLI claude workers
/// runs hermetically.
fn flow_router(
    runtime: Arc<crate::tools::NerveRuntime>,
    launcher: Arc<dyn crate::sandbox::SandboxLauncher>,
) -> (RuntimeDaemonRouter, Arc<Mutex<Vec<Value>>>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&output);
    let router = RuntimeDaemonRouter::new(
        runtime,
        ProviderRegistry::default(),
        crate::policy::Policy::default(),
        None,
        // Flow CLI workers need the delegate lift (same flag as delegate.start).
        true,
        launcher,
        move |value| {
            event_output.lock().expect("output lock").push(value);
        },
    );
    (router, output)
}

/// Collect the params of every `flow_*` event for `flow_id`, in arrival order.
fn flow_events(output: &Arc<Mutex<Vec<Value>>>, flow_id: &str) -> Vec<Value> {
    output
        .lock()
        .expect("output lock")
        .iter()
        .filter_map(|value| {
            let params = value.get("params")?;
            let ty = params.get("type")?.as_str()?;
            let is_flow = ty.starts_with("flow_");
            let matches = params.get("flow_id") == Some(&json!(flow_id));
            (is_flow && matches).then(|| params.clone())
        })
        .collect()
}

/// The ordered list of `flow_*` event `type`s seen for `flow_id`.
fn flow_event_types(output: &Arc<Mutex<Vec<Value>>>, flow_id: &str) -> Vec<String> {
    flow_events(output, flow_id)
        .iter()
        .filter_map(|p| p.get("type").and_then(Value::as_str).map(String::from))
        .collect()
}

/// A two-branch `Parallel` of CLI claude workers (join=all), as a `flow.start`
/// command payload.
fn parallel_flow_command() -> Value {
    json!({
        "kind": "flow.start",
        "workflow": {
            "schema_version": 1,
            "name": "fanout",
            "strategy": {
                "type": "parallel",
                "branches": [
                    { "worker": { "kind": "cli", "name": "claude" }, "task": "task one" },
                    { "worker": { "kind": "cli", "name": "claude" }, "task": "task two" }
                ],
                "join": { "kind": "all" }
            }
        }
    })
}

#[test]
fn flow_start_runs_parallel_and_emits_the_event_sequence() {
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());

    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "flow-1", "command": parallel_flow_command() }),
        ),
    );
    // The flow job goes terminal when the engine finishes both branches.
    wait_for_job_event(&output, "job_completed", "flow-1");

    let types = flow_event_types(&output, "flow-1");
    // The canonical sequence: started first, completed last.
    assert_eq!(types.first().map(String::as_str), Some("flow_started"));
    assert_eq!(types.last().map(String::as_str), Some("flow_completed"));
    // Both branch nodes started and finished, with at least one node-agent step each.
    let started = types.iter().filter(|t| *t == "flow_node_started").count();
    let finished = types.iter().filter(|t| *t == "flow_node_finished").count();
    assert_eq!(started, 2, "both branches started: {types:?}");
    assert_eq!(finished, 2, "both branches finished: {types:?}");
    assert!(
        types.iter().any(|t| t == "flow_node_agent"),
        "at least one node-agent step streamed: {types:?}"
    );
    // Two fan-out edges from the synthetic flow root, one per branch.
    let edges = types.iter().filter(|t| *t == "flow_edge").count();
    assert_eq!(edges, 2, "one edge per branch: {types:?}");

    // FlowNodeStarted carries the worker label + kind; finished carries ok + usage.
    let events = flow_events(&output, "flow-1");
    let node_started = events
        .iter()
        .find(|e| e["type"] == "flow_node_started")
        .expect("a node_started event");
    assert_eq!(node_started["worker"], "claude");
    assert_eq!(node_started["kind"], "cli");
    let node_finished = events
        .iter()
        .find(|e| e["type"] == "flow_node_finished")
        .expect("a node_finished event");
    assert_eq!(node_finished["ok"], true);
    assert_eq!(node_finished["usage"]["input_tokens"], 5);

    // The completed event + the job result both report the aggregated outcome.
    let completed = events
        .iter()
        .find(|e| e["type"] == "flow_completed")
        .expect("a flow_completed event");
    assert_eq!(completed["outcome"]["ok"], true);

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "flow-1", "include_result": true }),
        ),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "completed");
    assert_eq!(job["result"]["ok"], true);
    assert_eq!(job["result"]["name"], "fanout");
}

#[test]
fn flow_get_and_list_track_the_flow() {
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());

    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "flow-track",
                "command": {
                    "kind": "flow.start",
                    "workflow": {
                        "schema_version": 1,
                        "name": "single",
                        "strategy": {
                            "type": "single",
                            "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "go" }
                        }
                    }
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "flow-track");

    // flow.get returns the flow snapshot with its terminal outcome.
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({
                "job_id": "get-1",
                "command": { "kind": "flow.get", "flow_id": "flow-track" }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "get-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/get",
            json!({ "job_id": "get-1", "include_result": true }),
        ),
    );
    let flow = &response_with_id(&observed, json!(3))["result"]["job"]["result"]["flow"];
    assert_eq!(flow["flow_id"], "flow-track");
    assert_eq!(flow["name"], "single");
    assert_eq!(flow["status"], "finished");
    assert_eq!(flow["outcome"]["ok"], true);

    // flow.list includes the flow.
    dispatch(
        &router,
        &output,
        rpc(
            json!(4),
            "runtime/jobs/start",
            json!({ "job_id": "list-1", "command": { "kind": "flow.list" } }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "list-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(5),
            "runtime/jobs/get",
            json!({ "job_id": "list-1", "include_result": true }),
        ),
    );
    let flows = &response_with_id(&observed, json!(5))["result"]["job"]["result"]["flows"];
    let listed = flows.as_array().expect("flows array");
    assert!(
        listed.iter().any(|f| f["flow_id"] == "flow-track"),
        "flow.list includes the tracked flow: {flows}"
    );
}

#[test]
fn flow_get_unknown_errors() {
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "get-missing",
                "command": { "kind": "flow.get", "flow_id": "nope" }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "get-missing");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("nope")
    );
}

#[test]
fn flow_close_marks_flow_closed() {
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    // Run a flow to completion, then close it (an already-finished flow is still a
    // valid close target; close just fires the cancel token, which is harmless).
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "flow-close",
                "command": {
                    "kind": "flow.start",
                    "workflow": {
                        "schema_version": 1,
                        "name": "single",
                        "strategy": {
                            "type": "single",
                            "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "go" }
                        }
                    }
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "flow-close");

    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({
                "job_id": "close-1",
                "command": { "kind": "flow.close", "flow_id": "flow-close" }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "close-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/get",
            json!({ "job_id": "close-1", "include_result": true }),
        ),
    );
    let result = &response_with_id(&observed, json!(3))["result"]["job"]["result"];
    assert_eq!(result["closed"], true);
    assert_eq!(result["flow_id"], "flow-close");
}

#[test]
fn flow_steer_unknown_flow_errors() {
    // flow.steer routes through the full protocol path (parse → executor → flow
    // engine); an unknown flow id fails the steer job with a clear message.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-missing",
                "command": {
                    "kind": "flow.steer",
                    "flow_id": "nope",
                    "message": "go"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "steer-missing");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("nope")
    );
}

#[test]
fn flow_steer_finished_flow_errors_cleanly() {
    // A flow that has already finished has no live frontier; steering it fails with
    // a clear "finished" message rather than touching a dead session.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "flow-fin",
                "command": {
                    "kind": "flow.start",
                    "workflow": {
                        "schema_version": 1,
                        "name": "single",
                        "strategy": {
                            "type": "single",
                            "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "go" }
                        }
                    }
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "flow-fin");

    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-fin",
                "command": {
                    "kind": "flow.steer",
                    "flow_id": "flow-fin",
                    "message": "more please"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "steer-fin");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("finished"),
        "a finished flow is not steerable: {failed}"
    );
}

#[test]
fn flow_start_emits_budget_telemetry_end_to_end() {
    // A budgeted parallel flow (C3b) emits `budget_update` telemetry per debited
    // node end-to-end through the real protocol path. The fake claude reports
    // $0.001 + 8 tokens per turn, so a tiny budget warns then exhausts → a
    // `flow_decision` (budget_exhausted) and the flow completes not-ok.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());

    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "flow-budget",
                "command": {
                    "kind": "flow.start",
                    "workflow": {
                        "schema_version": 1,
                        "name": "budgeted-fanout",
                        // A USD cap below the flow's total reported cost: two $0.001
                        // branches total $0.002 > $0.0015, so the second debit
                        // exhausts the budget.
                        "budget": { "max_total_cost_usd": 0.0015 },
                        "strategy": {
                            "type": "parallel",
                            "branches": [
                                { "worker": { "kind": "cli", "name": "claude" }, "task": "one" },
                                { "worker": { "kind": "cli", "name": "claude" }, "task": "two" }
                            ],
                            "join": { "kind": "all" }
                        }
                    }
                }
            }),
        ),
    );
    // A budget-exhausted flow finishes (the job goes terminal; cancelled flows map
    // to job_failed, others to job_completed — either way the engine terminated).
    wait_for_job_terminal(&output, "flow-budget");

    // Budget telemetry (`budget_update` / `budget_warning` / `flow_decision`) is
    // flow-scoped via session_id()->flow_id, but `budget_*` types do NOT share the
    // `flow_` prefix, so collect by flow_id directly.
    let scoped = scoped_event_types(&output, "flow-budget");
    assert!(
        scoped.iter().any(|t| t == "budget_update"),
        "budget telemetry flowed: {scoped:?}"
    );
    assert!(
        scoped.iter().any(|t| t == "flow_decision"),
        "a budget_exhausted FlowDecision was recorded: {scoped:?}"
    );
    // The flow_decision is the budget_exhausted audit kind.
    let events = scoped_events(&output, "flow-budget");
    let decision = events
        .iter()
        .find(|e| e["type"] == "flow_decision")
        .expect("a flow_decision event");
    assert_eq!(decision["kind"]["kind"], "budget_exhausted");
}

/// Collect the params of every event scoped to `flow_id` (any `type`, including
/// the `budget_*` events that do not share the `flow_` prefix).
fn scoped_events(output: &Arc<Mutex<Vec<Value>>>, flow_id: &str) -> Vec<Value> {
    output
        .lock()
        .expect("output lock")
        .iter()
        .filter_map(|value| {
            let params = value.get("params")?;
            (params.get("flow_id") == Some(&json!(flow_id))).then(|| params.clone())
        })
        .collect()
}

/// The ordered list of all event `type`s scoped to `flow_id`.
fn scoped_event_types(output: &Arc<Mutex<Vec<Value>>>, flow_id: &str) -> Vec<String> {
    scoped_events(output, flow_id)
        .iter()
        .filter_map(|p| p.get("type").and_then(Value::as_str).map(String::from))
        .collect()
}

#[test]
fn flow_respond_to_unknown_request_is_a_noop_response() {
    // flow.respond reuses the ApprovalHub round-trip keyed by flow_id; responding to
    // a request that is not pending resolves to `responded: false` (no panic, no new
    // mechanism) — the same shape session.respond returns for a stale request.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "respond-1",
                "command": {
                    "kind": "flow.respond",
                    "flow_id": "ghost",
                    "request_id": "approval-99",
                    "decision": "allow"
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "respond-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "respond-1", "include_result": true }),
        ),
    );
    let result = &response_with_id(&observed, json!(2))["result"]["job"]["result"];
    assert_eq!(result["responded"], false);
    assert_eq!(result["flow_id"], "ghost");
}

#[test]
fn flow_replay_re_emits_the_recorded_flow_offline() {
    // THE AUDIT VERB end-to-end (C4, design §3/§4): RECORD a parallel flow (persisted
    // to .nerve/flows by the daemon), then `flow.replay` it BY FLOW ID. The replay
    // re-runs the engine over the recorded tape with NO live subprocess (a deny-all
    // approver, no launcher use) and re-emits the same Flow* sequence + outcome.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());

    // RECORD.
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "rec-1", "command": parallel_flow_command() }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "rec-1");
    let recorded_types: Vec<String> = flow_event_types(&output, "rec-1")
        .into_iter()
        .filter(|t| t != "flow_node_agent") // node-agent ordering across branches is timing-y; compare the lifecycle skeleton
        .collect();

    // REPLAY by flow id (a brand-new launcher proves no subprocess is spawned).
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({
                "job_id": "rep-1",
                "command": { "kind": "flow.replay", "flow_id": "rec-1" }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "rep-1");

    let replay_types: Vec<String> = flow_event_types(&output, "rep-1")
        .into_iter()
        .filter(|t| t != "flow_node_agent")
        .collect();
    assert_eq!(
        replay_types, recorded_types,
        "replay re-emits the recorded Flow* lifecycle sequence"
    );

    // The replay's outcome matches the record's (both completed ok).
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/get",
            json!({ "job_id": "rep-1", "include_result": true }),
        ),
    );
    let job = &response_with_id(&observed, json!(3))["result"]["job"];
    assert_eq!(job["status"], "completed");
    assert_eq!(job["result"]["ok"], true);
    assert_eq!(job["result"]["name"], "fanout");
}

#[test]
fn flow_replay_unknown_flow_errors_cleanly() {
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "rep-missing",
                "command": { "kind": "flow.replay", "flow_id": "never-recorded" }
            }),
        ),
    );
    // A replay of a flow with no persisted ledger fails the job cleanly (no panic).
    wait_for_job_terminal(&output, "rep-missing");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "rep-missing", "include_result": true }),
        ),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "failed");
}

/// A `flow.start` of a `VoteJudge` (two CLI candidates + a CLI judge, all the fake
/// claude) as a command payload. The judge's task interpolates the candidate outputs.
fn vote_judge_flow_command() -> Value {
    json!({
        "kind": "flow.start",
        "workflow": {
            "schema_version": 1,
            "name": "vote",
            "strategy": {
                "type": "vote_judge",
                "candidates": [
                    { "worker": { "kind": "cli", "name": "claude" }, "task": "draft A" },
                    { "worker": { "kind": "cli", "name": "claude" }, "task": "draft B" }
                ],
                "judge": {
                    "worker": { "kind": "cli", "name": "claude" },
                    "task": "judge {{cand-0}} vs {{cand-1}}"
                },
                "k": 2
            }
        }
    })
}

#[test]
fn flow_start_vote_judge_emits_the_decision_audit_trail() {
    // C5 end-to-end: a VoteJudge flow over real contained CLI workers emits the
    // `flow_decision` audit trail (a vote_tally then a judge_pick) through the protocol,
    // proving the C5 interpreter decisions reach a client unchanged.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());

    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "vote-1", "command": vote_judge_flow_command() }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "vote-1");

    let events = flow_events(&output, "vote-1");
    // The candidates + judge all started + finished (3 nodes).
    let started = events
        .iter()
        .filter(|e| e["type"] == "flow_node_started")
        .count();
    assert_eq!(started, 3, "two candidates + the judge started");
    // The audit trail: a vote_tally (2/2 ok, quorum 2 reached) then a judge_pick.
    let decisions: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "flow_decision")
        .collect();
    let tally = decisions
        .iter()
        .find(|e| e["kind"]["kind"] == "vote_tally")
        .expect("a vote_tally decision");
    assert_eq!(tally["kind"]["ok"], 2);
    assert_eq!(tally["kind"]["reached"], true);
    assert!(
        decisions.iter().any(|e| e["kind"]["kind"] == "judge_pick"),
        "a judge_pick decision: {decisions:?}"
    );
    // The vote → judge DAG edges emit at start (each candidate → judge).
    let to_judge = events
        .iter()
        .filter(|e| e["type"] == "flow_edge" && e["to"] == "judge")
        .count();
    assert_eq!(to_judge, 2, "each candidate edges into the judge");

    let completed = events
        .iter()
        .find(|e| e["type"] == "flow_completed")
        .expect("a flow_completed event");
    assert_eq!(completed["outcome"]["ok"], true);
}

#[test]
fn flow_start_rejects_a_zero_depth_hierarchy_at_start() {
    // The static safety gate (design §8): a malformed Hierarchical (max_depth 0) is
    // rejected at flow.start, before any worker spawns — the job fails cleanly.
    let fixture = runtime_with_file();
    let (router, output) = flow_router(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    let command = json!({
        "kind": "flow.start",
        "workflow": {
            "schema_version": 1,
            "name": "bad",
            "max_depth": 0,
            "strategy": {
                "type": "hierarchical",
                "planner": { "worker": { "kind": "cli", "name": "claude" }, "task": "plan" },
                "child": { "type": "single",
                           "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "work" } }
            }
        }
    });
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "bad-1", "command": command }),
        ),
    );
    wait_for_job_terminal(&output, "bad-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "bad-1", "include_result": true }),
        ),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "failed", "a zero-depth hierarchy is refused");
}
