//! DA-5a: daemon-level integration tests for the *persistent, steerable* claude
//! delegate session. Driven through the real router / job / live-session plumbing
//! with a **fake `claude`**: a tiny `/bin/sh` script that speaks the stream-json
//! protocol (system/init, an assistant + result per user line, staying alive until
//! stdin EOF). This exercises the real [`PersistentChild`] subprocess + stdin/stdout
//! pipes + the [`DelegateSession`] turn loop without the real CLI.
//!
//! [`PersistentChild`]: crate::sandbox::PersistentChild
//! [`DelegateSession`]: crate::delegate_session::DelegateSession

use super::super::router::RuntimeDaemonRouter;
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

/// A permission-proxying fake claude (DA-5b). For a user message containing the
/// marker `NEEDS_TOOL`, it emits a `can_use_tool` control_request for a `Bash`
/// tool call and then reads the next stdin line — the `control_response` Nerve
/// writes back — recording it verbatim to `record` so the test can assert the
/// exact bytes. On `allow` it emits a successful tool result; on `deny` it emits a
/// tool result with `is_error:true`. A message without the marker runs a plain
/// turn (no tool ask), so the AllowAlways case can drive a second `NEEDS_TOOL`
/// message that must NOT re-prompt.
const FAKE_CLAUDE_PERMISSION: &str = r#"#!/bin/sh
RECORD="__RECORD__"
printf '{"type":"system","subtype":"init","session_id":"perm-sess-1"}\n'
n=0
while IFS= read -r line; do
  n=$((n + 1))
  msg=$(printf '%s' "$line" | sed 's/.*"text":"\([^"]*\)".*/\1/')
  case "$msg" in
    *NEEDS_TOOL*)
      printf '{"type":"control_request","request_id":"perm-%s","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"echo hi"},"tool_use_id":"toolu_%s"}}\n' "$n" "$n"
      IFS= read -r resp
      printf '%s\n' "$resp" >> "$RECORD"
      case "$resp" in
        *'"behavior":"allow"'*)
          printf '{"type":"assistant","message":{"content":[{"type":"text","text":"ran bash for %s"}]}}\n' "$msg"
          printf '{"type":"result","subtype":"success","is_error":false,"result":"tool allowed for %s","session_id":"perm-sess-1","num_turns":%s,"total_cost_usd":0.001,"usage":{"input_tokens":5,"output_tokens":3}}\n' "$msg" "$n"
          ;;
        *)
          printf '{"type":"assistant","message":{"content":[{"type":"text","text":"bash blocked for %s"}]}}\n' "$msg"
          printf '{"type":"result","subtype":"success","is_error":true,"result":"tool denied for %s","session_id":"perm-sess-1","num_turns":%s,"total_cost_usd":0.001,"usage":{"input_tokens":5,"output_tokens":3}}\n' "$msg" "$n"
          ;;
      esac
      ;;
    *)
      printf '{"type":"assistant","message":{"content":[{"type":"text","text":"got %s"}]}}\n' "$msg"
      printf '{"type":"result","subtype":"success","is_error":false,"result":"reply to %s","session_id":"perm-sess-1","num_turns":%s,"total_cost_usd":0.001,"usage":{"input_tokens":5,"output_tokens":3}}\n' "$msg" "$n"
      ;;
  esac
done
"#;

/// A launcher that spawns [`FAKE_CLAUDE_PERMISSION`], recording each
/// `control_response` it reads back to a sidecar file the test can inspect.
struct FakePermissionLauncher {
    _dir: tempfile::TempDir,
    script: std::path::PathBuf,
    record: std::path::PathBuf,
}

impl FakePermissionLauncher {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("control-responses.ndjson");
        let script = dir.path().join("fake-claude-perm.sh");
        let body = FAKE_CLAUDE_PERMISSION.replace("__RECORD__", &record.display().to_string());
        std::fs::write(&script, body).expect("write fake claude perm");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude perm");
        Arc::new(Self {
            _dir: dir,
            script,
            record,
        })
    }

    /// The `control_response` lines the fake claude received, in order.
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("control_response json"))
            .collect()
    }
}

impl crate::sandbox::SandboxLauncher for FakePermissionLauncher {
    fn launch(
        &self,
        _spec: &crate::sandbox::CommandSpec,
        _policy: &crate::sandbox::SandboxPolicy,
        _cancel: &nerve_core::CancelToken,
    ) -> anyhow::Result<crate::sandbox::Output> {
        anyhow::bail!("fake permission launcher only supports the persistent path")
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

/// Find the first `approval_requested` event for `session_id`, polling until it
/// appears, and return its full params object (carrying `request_id`/`tool`/…).
fn wait_for_approval(output: &Arc<Mutex<Vec<Value>>>, session_id: &str) -> Value {
    for _ in 0..600 {
        let found = output
            .lock()
            .expect("output lock")
            .iter()
            .find_map(|value| {
                let params = value.get("params")?;
                let is_approval = params.get("type") == Some(&json!("approval_requested"));
                let matches = params.get("session_id") == Some(&json!(session_id));
                (is_approval && matches).then(|| params.clone())
            });
        if let Some(params) = found {
            return params;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for approval_requested on {session_id}");
}

/// Count `approval_requested` events seen for `session_id` so far.
fn approval_count(output: &Arc<Mutex<Vec<Value>>>, session_id: &str) -> usize {
    output
        .lock()
        .expect("output lock")
        .iter()
        .filter(|value| {
            value.get("params").is_some_and(|p| {
                p.get("type") == Some(&json!("approval_requested"))
                    && p.get("session_id") == Some(&json!(session_id))
            })
        })
        .count()
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
    // Generous budget (~6s): the permission tests spawn extra subprocesses in
    // parallel, so a tight window flakes under load. Returns as soon as the
    // progress appears, so the bound only affects failure latency.
    for _ in 0..600 {
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

// ---- DA-5b: permission proxying (can_use_tool → Nerve approval → control_response) ----

/// Start a permission-proxying delegated `claude` session and return the router,
/// the recording event output, and the concrete launcher (for asserting the
/// `control_response` bytes the fake claude received).
fn start_permission_session(
    job_id: &str,
    task: &str,
) -> (
    super::RuntimeFixture,
    RuntimeDaemonRouter,
    Arc<Mutex<Vec<Value>>>,
    Arc<FakePermissionLauncher>,
) {
    let fixture = runtime_with_file();
    let launcher = FakePermissionLauncher::new();
    let (router, output) = output_router_with_delegate(
        Arc::clone(&fixture.runtime),
        Arc::clone(&launcher) as Arc<dyn crate::sandbox::SandboxLauncher>,
    );
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": job_id,
                "command": { "kind": "delegate.start", "agent": "claude", "task": task }
            }),
        ),
    );
    // Return the fixture so the caller holds the root dir + runtime alive for the
    // session's life (the confined cwd points into the root); it drops at test end.
    (fixture, router, output, launcher)
}

/// Respond to a pending approval via `session.respond` (run as a job, the daemon's
/// only command surface), keyed by the delegate job id and the hub's request id —
/// the same round-trip the TUI uses to resolve an approval.
fn respond(
    router: &RuntimeDaemonRouter,
    output: &Arc<Mutex<Vec<Value>>>,
    session_id: &str,
    request_id: &str,
    decision: &str,
) {
    let job_id = format!("respond-{request_id}");
    dispatch(
        router,
        output,
        rpc(
            json!(100),
            "runtime/jobs/start",
            json!({
                "job_id": job_id,
                "command": {
                    "kind": "session.respond",
                    "session_id": session_id,
                    "request_id": request_id,
                    "decision": decision
                }
            }),
        ),
    );
}

#[test]
fn can_use_tool_emits_approval_and_allow_writes_control_response() {
    let (_fixture, router, output, launcher) =
        start_permission_session("live-perm-allow", "NEEDS_TOOL please run bash");

    // The can_use_tool ask surfaced as an approval_requested with the delegated
    // tool's real tier + a delegate-aware preview, keyed by the start-job id.
    let approval = wait_for_approval(&output, "live-perm-allow");
    assert_eq!(approval["tool"], "Bash");
    assert_eq!(approval["tier"], "exec");
    assert_eq!(approval["preview"], "claude wants to run Bash: echo hi");
    let request_id = approval["request_id"].as_str().expect("request id");

    // Allow it: the reader writes the allow control_response, the fake claude runs
    // the tool, and the turn completes ok.
    respond(&router, &output, "live-perm-allow", request_id, "allow");
    wait_for_progress_containing(&output, "live-perm-allow", "ran bash");

    // The exact control_response bytes the fake claude received: snake_case outer,
    // camelCase inner, echoing the requested input + tool_use_id.
    let received = wait_for_received(&launcher, 1);
    let resp = &received[0];
    assert_eq!(resp["type"], "control_response");
    assert_eq!(resp["response"]["subtype"], "success");
    let inner = &resp["response"]["response"];
    assert_eq!(inner["behavior"], "allow");
    assert_eq!(inner["updatedInput"], json!({ "command": "echo hi" }));
    assert_eq!(inner["toolUseID"], "toolu_1");

    close_and_assert_result(&router, &output, "live-perm-allow", "tool allowed", true);
}

#[test]
fn deny_writes_deny_control_response_and_tool_errors() {
    let (_fixture, router, output, launcher) =
        start_permission_session("live-perm-deny", "NEEDS_TOOL run bash");
    let approval = wait_for_approval(&output, "live-perm-deny");
    let request_id = approval["request_id"].as_str().expect("request id");

    respond(&router, &output, "live-perm-deny", request_id, "deny");
    wait_for_progress_containing(&output, "live-perm-deny", "bash blocked");

    let received = wait_for_received(&launcher, 1);
    let inner = &received[0]["response"]["response"];
    assert_eq!(inner["behavior"], "deny");
    // A plain deny does NOT interrupt the turn.
    assert!(inner.get("interrupt").is_none(), "{inner}");

    // The fake claude reported the tool as errored (is_error:true on its result).
    close_and_assert_result(&router, &output, "live-perm-deny", "tool denied", false);
}

#[test]
fn allow_always_remembers_and_skips_the_second_approval() {
    let (_fixture, router, output, launcher) =
        start_permission_session("live-perm-aa", "NEEDS_TOOL first");
    let approval = wait_for_approval(&output, "live-perm-aa");
    let request_id = approval["request_id"].as_str().expect("request id");
    respond(&router, &output, "live-perm-aa", request_id, "allow_always");
    wait_for_progress_containing(&output, "live-perm-aa", "ran bash for NEEDS_TOOL first");
    wait_for_received(&launcher, 1);

    // A second NEEDS_TOOL steer asks again on the wire, but the remembered
    // allow-always auto-allows it WITHOUT a second approval_requested.
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-perm-aa",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "live-perm-aa",
                    "message": "NEEDS_TOOL second"
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "steer-perm-aa");
    wait_for_progress_containing(&output, "live-perm-aa", "ran bash for NEEDS_TOOL second");

    // Both tool asks were allowed (two control_responses written), but the operator
    // was prompted exactly once.
    let received = wait_for_received(&launcher, 2);
    assert_eq!(received[0]["response"]["response"]["behavior"], "allow");
    assert_eq!(received[1]["response"]["response"]["behavior"], "allow");
    assert_eq!(approval_count(&output, "live-perm-aa"), 1);
}

#[test]
fn cancel_during_pending_approval_reaps_the_session() {
    let (_fixture, router, output, _launcher) =
        start_permission_session("live-perm-cancel", "NEEDS_TOOL run bash");
    // Wait for the pending approval, then cancel WITHOUT responding.
    wait_for_approval(&output, "live-perm-cancel");
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/cancel",
            json!({ "job_id": "live-perm-cancel" }),
        ),
    );
    // The cancel aborts the blocked approval wait, interrupts claude, and reaps the
    // session — the job finishes as cancelled rather than hanging on the approval.
    wait_for_job_event(&output, "job_cancelled", "live-perm-cancel");

    // The session is gone: a later steer reports unknown.
    dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/start",
            json!({
                "job_id": "steer-after-perm-cancel",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "live-perm-cancel",
                    "message": "still there?"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "steer-after-perm-cancel");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("no live delegated session")
    );
}

/// Poll until the fake claude has recorded at least `n` control_responses.
fn wait_for_received(launcher: &Arc<FakePermissionLauncher>, n: usize) -> Vec<Value> {
    for _ in 0..600 {
        let received = launcher.received();
        if received.len() >= n {
            return received;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {n} control_response(s)");
}

/// Close the parked start job and assert its terminal result (turn 1's outcome):
/// `result` contains `needle` and `ok` matches `expect_ok`.
fn close_and_assert_result(
    router: &RuntimeDaemonRouter,
    output: &Arc<Mutex<Vec<Value>>>,
    session_id: &str,
    needle: &str,
    expect_ok: bool,
) {
    dispatch(
        router,
        output,
        rpc(
            json!(200),
            "runtime/jobs/start",
            json!({
                "job_id": format!("close-{session_id}"),
                "command": { "kind": "delegate.close", "session_id": session_id }
            }),
        ),
    );
    wait_for_job_event(output, "job_completed", session_id);
    let observed = dispatch(
        router,
        output,
        rpc(
            json!(201),
            "runtime/jobs/get",
            json!({ "job_id": session_id, "include_result": true }),
        ),
    );
    let job = &response_with_id(&observed, json!(201))["result"]["job"];
    assert_eq!(job["status"], "completed");
    assert_eq!(job["result"]["ok"], expect_ok);
    let result = job["result"]["result"].as_str().unwrap_or_default();
    assert!(
        result.contains(needle),
        "result `{result}` lacks `{needle}`"
    );
}
