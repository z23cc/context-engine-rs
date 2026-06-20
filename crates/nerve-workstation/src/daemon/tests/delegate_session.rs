//! DA-5a: daemon-level integration tests for the *persistent, steerable* claude
//! delegate session. Driven through the real router / job / live-session plumbing
//! with a **fake `claude`**: a tiny `/bin/sh` script that speaks the stream-json
//! protocol (system/init, an assistant + result per user line, staying alive until
//! stdin EOF). This exercises the real [`PersistentChild`] subprocess + stdin/stdout
//! pipes + the [`DelegateSession`] turn loop without the real CLI.
//!
//! [`PersistentChild`]: crate::sandbox::PersistentChild
//! [`DelegateSession`]: crate::delegate_session::DelegateSession

use super::{
    Arc, Mutex, Value, dispatch, json, output_router, output_router_with_delegate,
    response_with_id, rpc, runtime_with_file, wait_for_job_event,
};
use std::os::unix::fs::PermissionsExt as _;

/// The fake-claude script body: emit an init line, then for each stream-json user
/// line read from stdin, echo the message text as an assistant line and a result
/// line, and stay alive (the `read` loop ends only on EOF → graceful exit).
const FAKE_CLAUDE: &str = r#"#!/bin/sh
printf '{"type":"system","subtype":"init","session_id":"fake-sess-1"}\n'
turn=0
while IFS= read -r line; do
  turn=$((turn + 1))
  msg=$(printf '%s' "$line" | sed 's/.*"text":"\([^"]*\)".*/\1/')
  printf '{"type":"assistant","message":{"content":[{"type":"text","text":"got %s"}]}}\n' "$msg"
  printf '{"type":"result","subtype":"success","is_error":false,"result":"reply to %s","session_id":"fake-sess-1","num_turns":%s,"total_cost_usd":0.001,"usage":{"input_tokens":5,"output_tokens":3}}\n' "$msg" "$turn"
done
"#;

/// A launcher whose `launch_persistent` ignores the requested `claude` program and
/// spawns the fake-claude script instead — so the persistent path runs a real
/// contained subprocess that speaks the protocol. Its one-shot `launch` is unused
/// (claude takes the live path), so it errors if ever called.
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
        // Rewrite the program to the fake script; keep the real containment policy.
        let spec = crate::sandbox::CommandSpec {
            command: self.script.display().to_string(),
            args: Vec::new(),
        };
        crate::sandbox::PersistentChild::spawn(&spec, policy)
    }
}

/// Collect the `delegate_progress` event texts seen so far for `session_id`.
fn progress_texts(output: &Arc<Mutex<Vec<Value>>>, session_id: &str) -> Vec<String> {
    output
        .lock()
        .expect("output lock")
        .iter()
        .filter_map(|value| {
            let params = value.get("params")?;
            let is_progress = params.get("type") == Some(&json!("delegate_progress"));
            let matches = params.get("job_id") == Some(&json!(session_id));
            (is_progress && matches).then(|| {
                params
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
        })
        .collect()
}

/// Spin until the live session is registered (turn 1 finished registering it), so
/// a steer doesn't race the parked start thread. Returns once a steer succeeds.
fn wait_for_progress_containing(output: &Arc<Mutex<Vec<Value>>>, session_id: &str, needle: &str) {
    for _ in 0..200 {
        if progress_texts(output, session_id)
            .iter()
            .any(|t| t.contains(needle))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for progress containing `{needle}` on {session_id}");
}

#[test]
fn live_session_start_runs_turn_one_then_steer_runs_turn_two() {
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());

    // Start a live claude session. The job stays running (parked) after turn 1.
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "live-1",
                "command": {
                    "kind": "delegate.start",
                    "agent": "claude",
                    "task": "first message"
                }
            }),
        ),
    );
    // Turn 1 streamed the assistant echo of the first message.
    wait_for_progress_containing(&output, "live-1", "got first message");

    // The start job is still running (parked for steering), not terminal.
    let observed = dispatch(
        &router,
        &output,
        rpc(json!(2), "runtime/jobs/get", json!({ "job_id": "live-1" })),
    );
    assert_eq!(
        response_with_id(&observed, json!(2))["result"]["job"]["status"],
        "running"
    );

    // Steer: the fake claude must see the SECOND message and run turn 2. The
    // session id == the start job id.
    dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-1",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "live-1",
                    "message": "second message"
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "steer-1");
    // Turn 2 streamed the echo of the second message (a fresh turn on the SAME
    // live process), and the steer job's result carried the turn outcome.
    wait_for_progress_containing(&output, "live-1", "got second message");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(4),
            "runtime/jobs/get",
            json!({ "job_id": "steer-1", "include_result": true }),
        ),
    );
    let steer_job = &response_with_id(&observed, json!(4))["result"]["job"];
    assert_eq!(steer_job["status"], "completed");
    assert_eq!(steer_job["result"]["agent"], "claude");
    assert_eq!(steer_job["result"]["ok"], true);
    assert_eq!(steer_job["result"]["result"], "reply to second message");
    assert_eq!(steer_job["result"]["session_id"], "live-1");

    // Close ends the session; the parked start job then finishes.
    dispatch(
        &router,
        &output,
        rpc(
            json!(5),
            "runtime/jobs/start",
            json!({
                "job_id": "close-1",
                "command": { "kind": "delegate.close", "session_id": "live-1" }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "close-1");
    // The parked start job completes once the session is closed; its result is
    // turn 1's outcome.
    wait_for_job_event(&output, "job_completed", "live-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(6),
            "runtime/jobs/get",
            json!({ "job_id": "live-1", "include_result": true }),
        ),
    );
    let start_job = &response_with_id(&observed, json!(6))["result"]["job"];
    assert_eq!(start_job["status"], "completed");
    assert_eq!(start_job["result"]["result"], "reply to first message");
}

#[test]
fn steer_unknown_session_errors() {
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-missing",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "nope",
                    "message": "hi"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "steer-missing");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("no live delegated session"), "{message}");
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn close_unknown_session_errors() {
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "close-missing",
                "command": { "kind": "delegate.close", "session_id": "ghost" }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "close-missing");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("no live delegated session"), "{message}");
}

#[test]
fn live_session_cancel_reaps_and_marks_cancelled() {
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeClaudeLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "live-cancel",
                "command": {
                    "kind": "delegate.start",
                    "agent": "claude",
                    "task": "hello"
                }
            }),
        ),
    );
    // Wait until turn 1 finished and the session is parked.
    wait_for_progress_containing(&output, "live-cancel", "got hello");

    // Cancel the parked start job: it must wake, reap the child, and finish as
    // cancelled (not stay running forever).
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/cancel",
            json!({ "job_id": "live-cancel" }),
        ),
    );
    wait_for_job_event(&output, "job_cancelled", "live-cancel");

    // The session is gone, so a subsequent steer reports unknown.
    dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-after-cancel",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "live-cancel",
                    "message": "still there?"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "steer-after-cancel");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("no live delegated session")
    );
}

#[test]
fn live_session_refused_when_delegation_disabled() {
    // The default daemon trust context (refusing launcher) must refuse to start a
    // live claude session, pointing at the --allow-delegate lift.
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "live-refused",
                "command": {
                    "kind": "delegate.start",
                    "agent": "claude",
                    "task": "investigate"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "live-refused");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("disabled"), "{message}");
    assert!(message.contains("--allow-delegate"), "{message}");
    assert!(message.contains("claude"), "{message}");
}
