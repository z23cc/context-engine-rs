//! Daemon-level integration tests for the DA-2 delegate runtime: trust-context
//! refusal, agent/cwd validation, and the streaming progress + outcome path.
//! Driven through the real router/job plumbing with a canned launcher so no
//! external agent CLI is spawned.

use super::{
    Arc, Mutex, Value, dispatch, json, output_router, output_router_with_delegate,
    response_with_id, rpc, runtime_with_file, wait_for_job_event,
};

#[test]
fn delegate_start_refused_by_default_trust_context() {
    // The default daemon refuses delegation (refusing launcher). The job reaches
    // a terminal failure whose message points at the --allow-delegate lift.
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "delegate-job",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "add a test"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "delegate-job");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message string");
    assert!(message.contains("disabled"), "{message}");
    assert!(message.contains("--allow-delegate"), "{message}");
    assert!(message.contains("codex"), "{message}");

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "delegate-job" }),
        ),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "failed");
    assert_eq!(job["command"], "delegate.start");
}

#[test]
fn delegate_start_unknown_agent_is_rejected() {
    // A bad agent name fails before any launcher is consulted, even with a real
    // (canned) launcher injected.
    let fixture = runtime_with_file();
    let (router, output) = output_router_with_delegate(
        Arc::clone(&fixture.runtime),
        Arc::new(CannedLauncher::ok("")),
    );
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "bad-agent",
                "command": { "kind": "delegate.start", "agent": "rovo", "task": "x" }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "bad-agent");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("unknown delegate agent"), "{message}");
}

#[test]
fn delegate_start_streams_progress_and_returns_outcome() {
    // A canned codex JSONL stream is replayed through the launcher's default
    // streaming path; the parser extracts the final result + usage and emits
    // DelegateProgress for each agent_message line.
    let fixture = runtime_with_file();
    let stream = concat!(
        r#"{"type":"item","item":{"type":"agent_message","text":"working"}}"#,
        "\n",
        r#"{"type":"item","item":{"type":"agent_message","text":"the answer"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":11,"output_tokens":7}}"#,
        "\n",
    );
    let (router, output) = output_router_with_delegate(
        Arc::clone(&fixture.runtime),
        Arc::new(CannedLauncher::ok(stream)),
    );
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "stream-job",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "do the thing"
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "stream-job");

    // DelegateProgress events carried the human-meaningful assistant text.
    let progress: Vec<String> = output
        .lock()
        .expect("output lock")
        .iter()
        .filter_map(|value| {
            let params = value.get("params")?;
            (params.get("type") == Some(&json!("delegate_progress"))).then(|| {
                params
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
        })
        .collect();
    assert_eq!(progress, vec!["working", "the answer"]);

    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/get",
            json!({ "job_id": "stream-job", "include_result": true }),
        ),
    );
    let job = &response_with_id(&observed, json!(2))["result"]["job"];
    assert_eq!(job["status"], "completed");
    let result = &job["result"];
    assert_eq!(result["agent"], "codex");
    assert_eq!(result["ok"], true);
    assert_eq!(result["result"], "the answer");
    assert_eq!(result["usage"]["input_tokens"], 11);
    assert_eq!(result["usage"]["output_tokens"], 7);
}

#[test]
fn delegate_start_rejects_cwd_escape() {
    let fixture = runtime_with_file();
    let (router, output) = output_router_with_delegate(
        Arc::clone(&fixture.runtime),
        Arc::new(CannedLauncher::ok("")),
    );
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "escape-job",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "x",
                    "cwd": "../etc"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "escape-job");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("cwd rejected"), "{message}");
    assert!(message.contains(".."), "{message}");
}

#[test]
fn delegate_start_cancel_short_circuits_to_cancelled() {
    // A canned launcher that flips the cancel token mid-run (as a real kill would
    // leave it). The job must terminate as `cancelled`, not `completed`, even
    // though the launcher returned an exit code.
    let fixture = runtime_with_file();
    let (router, output) = output_router_with_delegate(
        Arc::clone(&fixture.runtime),
        Arc::new(CannedLauncher::cancelling(
            r#"{"type":"item","item":{"type":"agent_message","text":"partial"}}"#,
        )),
    );
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "cancel-delegate",
                "command": { "kind": "delegate.start", "agent": "codex", "task": "x" }
            }),
        ),
    );
    let cancelled = wait_for_job_event(&output, "job_cancelled", "cancel-delegate");
    assert_eq!(cancelled["params"]["job_id"], "cancel-delegate");
}

/// A launcher that records the argv it was handed and replays a canned stdout
/// through the trait's default line-replay streaming path — exercising the
/// delegate parser without spawning a real process.
struct CannedLauncher {
    stdout: String,
    exit_code: Option<i32>,
    /// When set, the launcher cancels this token before returning, simulating a
    /// run that was interrupted (e.g. by `runtime/jobs/cancel`).
    cancel_on_run: bool,
    seen: Mutex<Option<crate::sandbox::CommandSpec>>,
}

impl CannedLauncher {
    fn ok(stdout: &str) -> Self {
        Self {
            stdout: stdout.to_string(),
            exit_code: Some(0),
            cancel_on_run: false,
            seen: Mutex::new(None),
        }
    }

    fn cancelling(stdout: &str) -> Self {
        Self {
            cancel_on_run: true,
            ..Self::ok(stdout)
        }
    }
}

impl crate::sandbox::SandboxLauncher for CannedLauncher {
    fn launch(
        &self,
        spec: &crate::sandbox::CommandSpec,
        _policy: &crate::sandbox::SandboxPolicy,
        cancel: &nerve_core::CancelToken,
    ) -> anyhow::Result<crate::sandbox::Output> {
        *self.seen.lock().expect("seen lock") = Some(crate::sandbox::CommandSpec {
            command: spec.command.clone(),
            args: spec.args.clone(),
        });
        if self.cancel_on_run {
            cancel.cancel();
        }
        Ok(crate::sandbox::Output {
            exit_code: self.exit_code,
            stdout: self.stdout.clone(),
            stderr: String::new(),
            timed_out: false,
        })
    }
}
