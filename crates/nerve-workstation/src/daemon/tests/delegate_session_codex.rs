//! DA-5c: daemon-level integration tests for the *persistent, steerable* codex
//! **app-server** delegate session. Driven through the real router / job /
//! live-session plumbing with a **fake `codex app-server`**: a `/bin/sh` script that
//! speaks the JSON-RPC-2.0-subset ndjson protocol (responds to `initialize`,
//! `thread/start`, `turn/start`; streams `item/agentMessage/delta` +
//! `turn/completed`; for a `NEEDS_TOOL` prompt emits a `requestApproval`
//! server-request and waits for the `{id,result:{decision}}` reply). This exercises
//! the real [`PersistentChild`] subprocess + the [`CodexSession`] turn loop without
//! the real CLI (which is not logged in here).
//!
//! [`PersistentChild`]: crate::sandbox::PersistentChild
//! [`CodexSession`]: crate::delegate_session_codex::CodexSession

use super::super::router::RuntimeDaemonRouter;
use super::{
    Arc, Mutex, Value, dispatch, json, live_session_gate, output_router,
    output_router_with_delegate, response_with_id, rpc, runtime_with_file, wait_for_job_event,
};
use std::os::unix::fs::PermissionsExt as _;

/// A minimal fake `codex app-server`. It extracts the JSON-RPC `id` and `method`
/// from each inbound line with `sed`, answers the handshake / `thread/start` /
/// `turn/start` requests, and streams a per-turn agent-message delta + a
/// `turn/completed`. For a `turn/start` whose input contains `NEEDS_TOOL` it first
/// emits a `commandExecution/requestApproval` server-request, reads back Nerve's
/// `{id,result:{decision}}` reply (recording it verbatim to `__RECORD__`), and
/// streams an allowed/declined message accordingly. Stays alive until stdin EOF.
///
/// Extra branches for the review-finding tests:
/// - `NEEDS_PERMS` → emits an `item/permissions/requestApproval` server-request
///   (finding A): the reply shape must honor a Deny (no `scope:"session"`).
/// - `TURN_ERR` → answers the `turn/start` request with a JSON-RPC `error` instead
///   of a `{turn}` result (finding E): the turn must fail fast, not wait 600s.
/// - any `turn/interrupt` line is recorded verbatim to `__RECORD__` (finding F): the
///   test asserts the interrupt carries the real (non-empty) `turnId`.
const FAKE_CODEX: &str = r#"#!/bin/sh
RECORD="__RECORD__"
emit() { printf '%s\n' "$1"; }
turn=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9-]*\).*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      emit "{\"id\":$id,\"result\":{\"serverInfo\":{\"name\":\"fake-codex\"}}}"
      ;;
    initialized)
      : # notification, no reply
      ;;
    thread/start)
      emit "{\"id\":$id,\"result\":{\"thread\":{\"id\":\"codex-thread-1\"}}}"
      ;;
    turn/start)
      turn=$((turn + 1))
      text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
      case "$text" in
        *TURN_ERR*)
          # Finding E: a turn/start that errors must fail fast, not loop to timeout.
          emit "{\"id\":$id,\"error\":{\"code\":-32000,\"message\":\"turn start rejected\"}}"
          continue
          ;;
      esac
      emit "{\"id\":$id,\"result\":{\"turn\":{\"id\":\"turn-$turn\"}}}"
      case "$text" in
        *NEEDS_PERMS*)
          emit "{\"id\":900$turn,\"method\":\"item/permissions/requestApproval\",\"params\":{\"threadId\":\"codex-thread-1\",\"turnId\":\"turn-$turn\",\"itemId\":\"item-$turn\"}}"
          IFS= read -r resp
          printf '%s\n' "$resp" >> "$RECORD"
          emit "{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"codex-thread-1\",\"turnId\":\"turn-$turn\",\"itemId\":\"item-$turn\",\"delta\":\"perms handled for $text\"}}"
          emit "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"codex-thread-1\",\"turn\":{\"id\":\"turn-$turn\",\"status\":\"completed\"}}}"
          ;;
        *NEEDS_TOOL*)
          emit "{\"id\":900$turn,\"method\":\"item/commandExecution/requestApproval\",\"params\":{\"threadId\":\"codex-thread-1\",\"turnId\":\"turn-$turn\",\"itemId\":\"item-$turn\",\"command\":\"echo hi\",\"cwd\":\"/w\"}}"
          IFS= read -r resp
          printf '%s\n' "$resp" >> "$RECORD"
          case "$resp" in
            *'"decision":"decline"'*|*'"decision":"cancel"'*)
              emit "{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"codex-thread-1\",\"turnId\":\"turn-$turn\",\"itemId\":\"item-$turn\",\"delta\":\"bash blocked for $text\"}}"
              emit "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"codex-thread-1\",\"turn\":{\"id\":\"turn-$turn\",\"status\":\"completed\"}}}"
              ;;
            *)
              emit "{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"codex-thread-1\",\"turnId\":\"turn-$turn\",\"itemId\":\"item-$turn\",\"delta\":\"ran bash for $text\"}}"
              emit "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"codex-thread-1\",\"turn\":{\"id\":\"turn-$turn\",\"status\":\"completed\"}}}"
              ;;
          esac
          ;;
        *)
          emit "{\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"codex-thread-1\",\"turnId\":\"turn-$turn\",\"itemId\":\"item-$turn\",\"delta\":\"got $text\"}}"
          emit "{\"method\":\"thread/tokenUsage/updated\",\"params\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3,\"cached_input_tokens\":1}}}"
          emit "{\"method\":\"turn/completed\",\"params\":{\"threadId\":\"codex-thread-1\",\"turn\":{\"id\":\"turn-$turn\",\"status\":\"completed\"}}}"
          ;;
      esac
      ;;
    turn/interrupt)
      # Finding F: record the interrupt so the test can assert its turnId is real.
      printf '%s\n' "$line" >> "$RECORD"
      ;;
    *)
      : # unknown method, ignore
      ;;
  esac
done
"#;

/// A launcher whose `launch_persistent` spawns the fake codex app-server instead of
/// the real `codex` binary, recording each approval reply it reads back to a sidecar
/// file. Its one-shot `launch` is unused (codex takes the live path), so it errors.
struct FakeCodexLauncher {
    _dir: tempfile::TempDir,
    script: std::path::PathBuf,
    record: std::path::PathBuf,
}

impl FakeCodexLauncher {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let record = dir.path().join("approval-replies.ndjson");
        let script = dir.path().join("fake-codex.sh");
        let body = FAKE_CODEX.replace("__RECORD__", &record.display().to_string());
        std::fs::write(&script, body).expect("write fake codex");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake codex");
        Arc::new(Self {
            _dir: dir,
            script,
            record,
        })
    }

    /// The approval reply lines the fake codex received, in order.
    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("approval reply json"))
            .collect()
    }
}

impl crate::sandbox::SandboxLauncher for FakeCodexLauncher {
    fn launch(
        &self,
        _spec: &crate::sandbox::CommandSpec,
        _policy: &crate::sandbox::SandboxPolicy,
        _cancel: &nerve_core::CancelToken,
    ) -> anyhow::Result<crate::sandbox::Output> {
        anyhow::bail!("fake codex launcher only supports the persistent path")
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

// ---- shared poll helpers (codex-keyed copies of the claude-session helpers) ----

fn wait_for_approval(output: &Arc<Mutex<Vec<Value>>>, session_id: &str) -> Value {
    for _ in 0..3000 {
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

fn wait_for_progress_containing(output: &Arc<Mutex<Vec<Value>>>, session_id: &str, needle: &str) {
    for _ in 0..3000 {
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

fn wait_for_received(launcher: &Arc<FakeCodexLauncher>, n: usize) -> Vec<Value> {
    for _ in 0..3000 {
        let received = launcher.received();
        if received.len() >= n {
            return received;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {n} approval reply(ies)");
}

#[test]
fn codex_live_session_start_runs_turn_one_then_steer_runs_turn_two() {
    let _gate = live_session_gate();
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeCodexLauncher::new());

    // Start a live codex session. The job stays running (parked) after turn 1.
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-1",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "first"
                }
            }),
        ),
    );
    // Turn 1 streamed the agent-message echo of the first message.
    wait_for_progress_containing(&output, "codex-1", "got first");

    // The start job is still running (parked for steering), not terminal.
    let observed = dispatch(
        &router,
        &output,
        rpc(json!(2), "runtime/jobs/get", json!({ "job_id": "codex-1" })),
    );
    assert_eq!(
        response_with_id(&observed, json!(2))["result"]["job"]["status"],
        "running"
    );

    // Steer: the fake codex sees the SECOND message and runs turn 2 on the same
    // thread. The session id is the captured thread id.
    dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-steer-1",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "codex-1",
                    "message": "second"
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "codex-steer-1");
    wait_for_progress_containing(&output, "codex-1", "got second");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(4),
            "runtime/jobs/get",
            json!({ "job_id": "codex-steer-1", "include_result": true }),
        ),
    );
    let steer_job = &response_with_id(&observed, json!(4))["result"]["job"];
    assert_eq!(steer_job["status"], "completed");
    assert_eq!(steer_job["result"]["agent"], "codex");
    assert_eq!(steer_job["result"]["ok"], true);
    assert_eq!(steer_job["result"]["result"], "got second");
    assert_eq!(steer_job["result"]["session_id"], "codex-1");

    // Close ends the session; the parked start job then finishes with turn 1's
    // outcome (the captured thread id rides the result).
    dispatch(
        &router,
        &output,
        rpc(
            json!(5),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-close-1",
                "command": { "kind": "delegate.close", "session_id": "codex-1" }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "codex-close-1");
    wait_for_job_event(&output, "job_completed", "codex-1");
    let observed = dispatch(
        &router,
        &output,
        rpc(
            json!(6),
            "runtime/jobs/get",
            json!({ "job_id": "codex-1", "include_result": true }),
        ),
    );
    let start_job = &response_with_id(&observed, json!(6))["result"]["job"];
    assert_eq!(start_job["status"], "completed");
    assert_eq!(start_job["result"]["agent"], "codex");
    assert_eq!(start_job["result"]["result"], "got first");
    assert_eq!(start_job["result"]["session_id"], "codex-thread-1");
}

#[test]
fn codex_live_session_refused_when_delegation_disabled() {
    // The default daemon trust context (refusing launcher) must refuse to start a
    // live codex session, pointing at the --allow-delegate lift.
    let fixture = runtime_with_file();
    let (router, output) = output_router(Arc::clone(&fixture.runtime));
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-refused",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "investigate"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "codex-refused");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("disabled"), "{message}");
    assert!(message.contains("--allow-delegate"), "{message}");
    assert!(message.contains("codex"), "{message}");
}

#[test]
fn codex_steer_unknown_session_errors() {
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeCodexLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-steer-missing",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "nope",
                    "message": "hi"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "codex-steer-missing");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(message.contains("no live delegated session"), "{message}");
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn codex_live_session_cancel_reaps_and_marks_cancelled() {
    let _gate = live_session_gate();
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeCodexLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-cancel",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "hello"
                }
            }),
        ),
    );
    wait_for_progress_containing(&output, "codex-cancel", "got hello");

    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/cancel",
            json!({ "job_id": "codex-cancel" }),
        ),
    );
    wait_for_job_event(&output, "job_cancelled", "codex-cancel");

    // The session is gone, so a subsequent steer reports unknown.
    dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-steer-after-cancel",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "codex-cancel",
                    "message": "still there?"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "codex-steer-after-cancel");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("no live delegated session")
    );
}

// ---- DA-5c: permission proxying (requestApproval → Nerve approval → reply) ----

/// Start a permission-proxying delegated codex session and return the fixture,
/// router, recording event output, and the concrete launcher (for asserting the
/// `{id,result:{decision}}` reply bytes the fake codex received).
fn start_permission_session(
    job_id: &str,
    task: &str,
) -> (
    super::RuntimeFixture,
    RuntimeDaemonRouter,
    Arc<Mutex<Vec<Value>>>,
    Arc<FakeCodexLauncher>,
) {
    let fixture = runtime_with_file();
    let launcher = FakeCodexLauncher::new();
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
                "command": { "kind": "delegate.start", "agent": "codex", "task": task }
            }),
        ),
    );
    (fixture, router, output, launcher)
}

/// Respond to a pending approval via `session.respond` (run as a job), keyed by the
/// delegate job id and the hub's request id — the same round-trip the TUI uses.
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
fn codex_request_approval_emits_approval_and_allow_writes_accept() {
    let _gate = live_session_gate();
    let (_fixture, router, output, launcher) =
        start_permission_session("codex-perm-allow", "NEEDS_TOOL please run bash");

    // The requestApproval surfaced as an approval_requested with the codex tool's
    // tier + a codex-labeled preview, keyed by the start-job id.
    let approval = wait_for_approval(&output, "codex-perm-allow");
    assert_eq!(approval["tool"], "Bash");
    assert_eq!(approval["tier"], "exec");
    assert_eq!(approval["preview"], "codex wants to run Bash: echo hi");
    let request_id = approval["request_id"].as_str().expect("request id");

    // Allow it: the reader replies accept, the fake codex runs the tool, the turn
    // completes ok.
    respond(&router, &output, "codex-perm-allow", request_id, "allow");
    wait_for_progress_containing(&output, "codex-perm-allow", "ran bash");

    // The exact reply bytes the fake codex received: a JSON-RPC response with the
    // server-request id echoed and result.decision == accept.
    let received = wait_for_received(&launcher, 1);
    let reply = &received[0];
    assert_eq!(reply["id"], json!(9001));
    assert_eq!(reply["result"]["decision"], "accept");

    close_and_assert_result(&router, &output, "codex-perm-allow", "ran bash", true);
}

#[test]
fn codex_deny_writes_decline_reply() {
    let _gate = live_session_gate();
    let (_fixture, router, output, launcher) =
        start_permission_session("codex-perm-deny", "NEEDS_TOOL run bash");
    let approval = wait_for_approval(&output, "codex-perm-deny");
    let request_id = approval["request_id"].as_str().expect("request id");

    respond(&router, &output, "codex-perm-deny", request_id, "deny");
    wait_for_progress_containing(&output, "codex-perm-deny", "bash blocked");

    let received = wait_for_received(&launcher, 1);
    assert_eq!(received[0]["result"]["decision"], "decline");

    close_and_assert_result(&router, &output, "codex-perm-deny", "bash blocked", true);
}

#[test]
fn codex_allow_always_maps_to_accept_for_session_and_skips_second_approval() {
    let _gate = live_session_gate();
    let (_fixture, router, output, launcher) =
        start_permission_session("codex-perm-aa", "NEEDS_TOOL first");
    let approval = wait_for_approval(&output, "codex-perm-aa");
    let request_id = approval["request_id"].as_str().expect("request id");
    respond(
        &router,
        &output,
        "codex-perm-aa",
        request_id,
        "allow_always",
    );
    wait_for_progress_containing(&output, "codex-perm-aa", "ran bash for NEEDS_TOOL first");
    let received = wait_for_received(&launcher, 1);
    // The first allow-always maps to acceptForSession.
    assert_eq!(received[0]["result"]["decision"], "acceptForSession");

    // A second NEEDS_TOOL steer asks again on the wire, but the remembered
    // allow-always auto-accepts it WITHOUT a second approval_requested.
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-steer-perm-aa",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "codex-perm-aa",
                    "message": "NEEDS_TOOL second"
                }
            }),
        ),
    );
    wait_for_job_event(&output, "job_completed", "codex-steer-perm-aa");
    wait_for_progress_containing(&output, "codex-perm-aa", "ran bash for NEEDS_TOOL second");

    let received = wait_for_received(&launcher, 2);
    assert_eq!(received[0]["result"]["decision"], "acceptForSession");
    // The second was auto-served from memory → a plain accept, no second prompt.
    assert_eq!(received[1]["result"]["decision"], "accept");
    assert_eq!(approval_count(&output, "codex-perm-aa"), 1);
}

#[test]
fn codex_cancel_during_pending_approval_reaps_the_session() {
    let _gate = live_session_gate();
    let (_fixture, router, output, _launcher) =
        start_permission_session("codex-perm-cancel", "NEEDS_TOOL run bash");
    wait_for_approval(&output, "codex-perm-cancel");
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/cancel",
            json!({ "job_id": "codex-perm-cancel" }),
        ),
    );
    wait_for_job_event(&output, "job_cancelled", "codex-perm-cancel");

    dispatch(
        &router,
        &output,
        rpc(
            json!(3),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-steer-after-perm-cancel",
                "command": {
                    "kind": "delegate.steer",
                    "session_id": "codex-perm-cancel",
                    "message": "still there?"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "codex-steer-after-perm-cancel");
    assert!(
        failed["params"]["error"]["message"]
            .as_str()
            .expect("msg")
            .contains("no live delegated session")
    );
}

#[test]
fn codex_permission_deny_reply_carries_no_session_scope() {
    // Finding A: a DENY of a codex `item/permissions/requestApproval` must NOT
    // become a session-wide grant. The recorded reply must omit `scope:"session"`.
    let _gate = live_session_gate();
    let (_fixture, router, output, launcher) =
        start_permission_session("codex-perms-deny", "NEEDS_PERMS please grant");
    let approval = wait_for_approval(&output, "codex-perms-deny");
    // The permissions ask is labeled and gated at the safe (exec) tier.
    assert_eq!(approval["tool"], "permissions");
    assert_eq!(approval["tier"], "exec");
    let request_id = approval["request_id"].as_str().expect("request id");

    // Deny it.
    respond(&router, &output, "codex-perms-deny", request_id, "deny");

    // The exact reply bytes the fake codex received for the permissions ask: a
    // non-grant body with NO session scope (its absence is the deny).
    let received = wait_for_received(&launcher, 1);
    let reply = &received[0];
    assert_eq!(reply["id"], json!(9001));
    assert!(
        reply["result"].get("scope").is_none(),
        "a denied permissions reply must not carry a session scope: {reply}"
    );
    // The non-grant shape: empty permissions, no auto-review, no scope.
    assert!(reply["result"]["permissions"].is_object());
    assert_eq!(reply["result"]["strictAutoReview"], false);
}

#[test]
fn codex_turn_start_error_fails_fast() {
    // Finding E: a `turn/start` RESPONSE carrying an error must fail the turn
    // immediately rather than looping to the 600s per-turn timeout. The job
    // completing at all (well within the test budget) is the proof of "fast".
    let _gate = live_session_gate();
    let fixture = runtime_with_file();
    let (router, output) =
        output_router_with_delegate(Arc::clone(&fixture.runtime), FakeCodexLauncher::new());
    dispatch(
        &router,
        &output,
        rpc(
            json!(1),
            "runtime/jobs/start",
            json!({
                "job_id": "codex-turn-err",
                "command": {
                    "kind": "delegate.start",
                    "agent": "codex",
                    "task": "TURN_ERR boom"
                }
            }),
        ),
    );
    let failed = wait_for_job_event(&output, "job_failed", "codex-turn-err");
    let message = failed["params"]["error"]["message"]
        .as_str()
        .expect("error message");
    assert!(
        message.contains("turn/start error") || message.contains("turn start rejected"),
        "{message}"
    );
}

#[test]
fn codex_deny_under_cancel_interrupts_with_real_turn_id() {
    // Finding F: a deny-under-cancel must send `turn/interrupt` with the REAL turn
    // id (captured from the approval request / turn/start response), never `""` —
    // an empty turnId can't match the in-flight turn on real codex.
    let _gate = live_session_gate();
    let (_fixture, router, output, launcher) =
        start_permission_session("codex-cancel-interrupt", "NEEDS_TOOL run bash");
    // Wait for the pending approval, then cancel WITHOUT responding: the blocked
    // approval resolves to a deny-under-cancel, which interrupts the turn.
    wait_for_approval(&output, "codex-cancel-interrupt");
    dispatch(
        &router,
        &output,
        rpc(
            json!(2),
            "runtime/jobs/cancel",
            json!({ "job_id": "codex-cancel-interrupt" }),
        ),
    );
    wait_for_job_event(&output, "job_cancelled", "codex-cancel-interrupt");

    // The recording holds the decline reply AND the turn/interrupt frame. Find the
    // interrupt and assert it names the real turn (turn-1), not an empty id.
    let interrupt = wait_for_interrupt(&launcher);
    assert_eq!(
        interrupt["params"]["turnId"], "turn-1",
        "turn/interrupt must target the real turn: {interrupt}"
    );
    assert_eq!(interrupt["method"], "turn/interrupt");
}

/// Poll the recording for the `turn/interrupt` frame the fake codex received.
fn wait_for_interrupt(launcher: &Arc<FakeCodexLauncher>) -> Value {
    for _ in 0..3000 {
        if let Some(frame) = launcher
            .received()
            .into_iter()
            .find(|v| v.get("method") == Some(&json!("turn/interrupt")))
        {
            return frame;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for a recorded turn/interrupt");
}

/// Close the parked start job and assert its terminal result (turn 1's outcome).
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
