mod delegate;
#[cfg(unix)]
mod delegate_session;

use super::router::{RuntimeDaemonRouter, runtime_event_notification};
use crate::jobs::JobManager;
use crate::providers::ProviderRegistry;
use crate::rpc::RpcMessage;
use crate::{
    tools,
    workspace::{args_with, registry},
};
use nerve_runtime::RuntimeEvent;
use serde_json::{Value, json};
use std::fs;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

struct RuntimeFixture {
    _root: tempfile::TempDir,
    runtime: Arc<tools::NerveRuntime>,
}

fn runtime_with_file() -> RuntimeFixture {
    let root = tempfile::tempdir().expect("tempdir");
    fs::write(root.path().join("notes.txt"), "alpha beta\n").expect("write notes");
    let runtime = tools::runtime(
        registry(&args_with(vec![root.path().to_path_buf()], Vec::new())).expect("registry"),
    );
    RuntimeFixture {
        _root: root,
        runtime: Arc::new(runtime),
    }
}

fn rpc(id: impl Into<Option<Value>>, method: &str, params: Value) -> RpcMessage {
    RpcMessage {
        id: id.into(),
        method: method.to_string(),
        params,
    }
}

fn output_router(
    runtime: Arc<tools::NerveRuntime>,
) -> (RuntimeDaemonRouter, Arc<Mutex<Vec<Value>>>) {
    // Default daemon trust context: delegation refused.
    output_router_with_delegate(runtime, crate::sandbox::refuse_launcher())
}

fn output_router_with_delegate(
    runtime: Arc<tools::NerveRuntime>,
    delegate_launcher: Arc<dyn crate::sandbox::SandboxLauncher>,
) -> (RuntimeDaemonRouter, Arc<Mutex<Vec<Value>>>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&output);
    let router = RuntimeDaemonRouter::new(
        runtime,
        ProviderRegistry::default(),
        crate::policy::Policy::default(),
        None,
        // These helpers exercise the `delegate.start` JOB path, whose lift is the
        // launcher (not the session-tool bool); pass `false` for the bool since the
        // session chat-tool path isn't under test here.
        false,
        delegate_launcher,
        move |value| {
            event_output.lock().expect("output lock").push(value);
        },
    );
    (router, output)
}

fn dispatch(
    router: &RuntimeDaemonRouter,
    output: &Arc<Mutex<Vec<Value>>>,
    message: RpcMessage,
) -> Vec<Value> {
    router
        .handle_message(message, |value| {
            output.lock().expect("output lock").push(value);
            Ok(())
        })
        .expect("dispatch");
    output.lock().expect("output lock").clone()
}

fn response_with_id(output: &[Value], id: Value) -> &Value {
    output
        .iter()
        .find(|value| value.get("id") == Some(&id))
        .expect("response id")
}

fn wait_for_job_event(output: &Arc<Mutex<Vec<Value>>>, event_type: &str, job_id: &str) -> Value {
    for _ in 0..100 {
        let found = output
            .lock()
            .expect("output lock")
            .iter()
            .find_map(|value| {
                let params = value.get("params")?;
                let matches_type = params.get("type") == Some(&json!(event_type));
                let matches_job = params.get("job_id") == Some(&json!(job_id));
                (matches_type && matches_job).then(|| value.clone())
            });
        if let Some(value) = found {
            return value;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {event_type} on {job_id}");
}

#[test]
fn initialize_returns_runtime_info() {
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    let responses = dispatch(&router, &output, rpc(json!(1), "initialize", json!({})));
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["result"]["protocol"], "nerve-runtime");
    assert_eq!(responses[0]["result"]["protocolVersion"], "3");
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "nerve");
    assert_eq!(
        responses[0]["result"]["capabilities"]["jobs"]["methods"][0],
        "runtime/jobs/start"
    );
    assert_eq!(
        responses[0]["result"]["capabilities"]["jobs"]["commandKinds"][0],
        "ping"
    );
}

#[test]
fn runtime_command_is_not_supported() {
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    let responses = dispatch(
        &router,
        &output,
        rpc(
            json!(7),
            "runtime/command",
            json!({ "command": { "kind": "ping" } }),
        ),
    );

    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["error"]["code"], -32601);
}

#[test]
fn job_start_get_and_list_track_completed_job() {
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "job-ok", "command": { "kind": "ping" } }),
        ),
    );
    assert_eq!(
        response_with_id(&observed, json!(1))["result"]["job"]["status"],
        "running"
    );
    assert_eq!(observed[0]["params"]["type"], "job_started");
    wait_for_job_event(&output, "job_completed", "job-ok");

    let observed = dispatch(
        &router,
        &output,
        rpc(json!(2), "runtime/jobs/get", json!({ "job_id": "job-ok" })),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "completed");
    assert_eq!(job["result"]["status"], "ok");

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/list",
            json!({ "include_terminal": true, "include_results": false }),
        ),
    );
    let listed = &response_with_id(&observed, json!(3))["result"]["jobs"];
    assert_eq!(listed.as_array().expect("jobs").len(), 1);
    assert_eq!(listed[0]["job_id"], "job-ok");
    assert!(listed[0]["result"].is_null());

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(4),
            "runtime/jobs/list",
            json!({ "include_terminal": false }),
        ),
    );
    assert_eq!(
        response_with_id(&observed, json!(4))["result"]["jobs"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn job_duplicate_and_unknown_errors_use_protocol_codes() {
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "same", "command": { "kind": "ping" } }),
        ),
    );
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({ "job_id": "same", "command": { "kind": "ping" } }),
        ),
    );
    assert_eq!(
        response_with_id(&observed, json!(2))["error"]["code"],
        -32009
    );

    let observed = dispatch(
        &router,
        &output,
        rpc(json!(3), "runtime/jobs/get", json!({ "job_id": "missing" })),
    );
    assert_eq!(
        response_with_id(&observed, json!(3))["error"]["code"],
        -32004
    );

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(4),
            "runtime/jobs/cancel",
            json!({ "job_id": "missing" }),
        ),
    );
    assert_eq!(
        response_with_id(&observed, json!(4))["error"]["code"],
        -32004
    );
}

#[test]
fn job_failure_stores_error_and_emits_terminal_event() {
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "job-fail",
                "command": { "kind": "tool.call", "name": "missing_tool" }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "job-fail");
    assert!(failed["params"]["error"]["message"].is_string());

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "job-fail" }),
        ),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "failed");
    assert!(job["error"]["message"].is_string());
}

#[test]
fn job_cancel_requests_token_and_emits_cancelled() {
    let fixture = runtime_with_file();
    let output = Arc::new(Mutex::new(Vec::new()));
    let event_output = Arc::clone(&output);
    let (progress_tx, progress_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let jobs = Arc::new(JobManager::new(
        Arc::clone(&fixture.runtime),
        ProviderRegistry::default(),
        crate::policy::Policy::default(),
        None,
        move |event| {
            let block = matches!(event, RuntimeEvent::JobProgress { ref job_id, .. } if job_id == "cancel-me");
            event_output
                .lock()
                .expect("output lock")
                .push(runtime_event_notification(0, event));
            if block {
                progress_tx.send(()).expect("progress send");
                release_rx
                    .lock()
                    .expect("release lock")
                    .recv()
                    .expect("release");
            }
        },
    ));
    let router = RuntimeDaemonRouter::with_jobs(jobs);

    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({ "job_id": "cancel-me", "command": { "kind": "ping" } }),
        ),
    );
    progress_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("job progress");

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/cancel",
            json!({ "job_id": "cancel-me" }),
        ),
    );
    let cancel = &response_with_id(&observed, json!(2))["result"];
    assert_eq!(cancel["cancellation_requested"], true);
    assert_eq!(cancel["job"]["status"], "cancelling");
    wait_for_job_event(&output, "job_cancel_requested", "cancel-me");
    release_tx.send(()).expect("release job");
    wait_for_job_event(&output, "job_cancelled", "cancel-me");

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/get",
            json!({ "job_id": "cancel-me" }),
        ),
    );
    assert_eq!(
        response_with_id(&observed, json!(3))["result"]["job"]["status"],
        "cancelled"
    );
}

#[test]
fn job_start_notification_emits_events_without_response() {
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    dispatch(
        &router,
        &output,
        rpc(
            None,
            "runtime/jobs/start",
            json!({ "job_id": "notify", "command": { "kind": "ping" } }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "notify");
    let observed = output.lock().expect("output lock").clone();
    assert!(observed.iter().all(|value| value.get("id").is_none()));
    assert!(
        observed
            .iter()
            .any(|value| value["params"]["type"] == "job_started")
    );
    assert!(
        observed
            .iter()
            .any(|value| value["params"]["type"] == "job_progress")
    );
    assert!(
        observed
            .iter()
            .any(|value| value["params"]["type"] == "job_completed")
    );
}
