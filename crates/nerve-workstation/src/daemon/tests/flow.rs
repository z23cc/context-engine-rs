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
    Arc, Mutex, Value, dispatch, json, response_with_id, rpc, runtime_with_file, wait_for_job_event,
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
