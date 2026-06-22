//! DA-5a: the *persistent, steerable* delegate session runtime.
//!
//! [`delegate_runtime`](crate::delegate_runtime) (DA-2) runs an external agent CLI
//! **once** — feed the task on stdin, stream until the process exits, parse a
//! single outcome. This module is the live multi-turn shape: a `claude` process
//! kept alive across turns, driven over an **open** stdin in the `stream-json`
//! input format, so the same session can be steered with follow-up messages.
//!
//! ## The pinned claude streaming protocol (verified, live)
//!
//! Spawn (cwd is the confined child working dir; the launcher forces it):
//! ```text
//! claude -p --verbose --output-format stream-json --input-format stream-json \
//!        --permission-mode <plan|acceptEdits|bypassPermissions> [--model <m>] [--add-dir <cwd>]
//! ```
//! The process **stays alive** as long as stdin is open and exits on stdin EOF.
//!
//! Each user message (the initial prompt *and* every steer) is one framed write
//! (body + `\n`, flushed):
//! ```json
//! {"type":"user","message":{"role":"user","content":[{"type":"text","text":"<MSG>"}]},"parent_tool_use_id":null}
//! ```
//!
//! Reading NDJSON lines: `system`(subtype `init` → carries `session_id`),
//! `assistant`(message.content[] text/thinking/tool_use blocks → stream the text),
//! `user`(tool_result echoes), and `result`(`{type:result, subtype, is_error,
//! result, num_turns, session_id, total_cost_usd, usage}`) = **turn done**, after
//! which the process awaits the next user line.
//!
//! On cancel while a turn is in flight, an `interrupt` is sent as a control
//! request; the CLI replies with a control_response.
//!
//! ## DA-5b — permission proxying
//!
//! When the session is given a [`DelegateProxy`](crate::delegate_proxy::DelegateProxy)
//! (an approver is available), the child is started in **proxied mode**:
//! `--permission-prompt-tool stdio --permission-mode default`, so claude asks
//! before each tool use instead of auto-running. A `can_use_tool` control_request
//! arriving in the read loop is routed to Nerve's approval hub and answered with a
//! `control_response` (see [`crate::delegate_proxy`]). Without a proxy (a pure CLI
//! run with no interactive approver) we keep the DA-5a autonomy-driven
//! `--permission-mode` and a stray `can_use_tool` is logged-and-ignored.

use crate::delegate_proxy::DelegateProxy;
use crate::delegate_runtime::DelegateUsage;
use crate::sandbox::{CommandSpec, PersistentChild, SandboxLauncher};
use nerve_core::CancelToken;
use nerve_runtime::DelegateAutonomy;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;

/// How long to wait for a turn's `result` line before treating the session as
/// stalled. A live session has no per-command wall-clock (it is bounded by the
/// caller's close), but a single turn that never produces a `result` must not
/// hang a job thread forever.
const TURN_TIMEOUT: Duration = Duration::from_secs(600);

/// How often the turn loop wakes to re-check cancellation and child liveness
/// while blocking on the next stdout line.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The outcome of one turn of a live delegated session: the assistant's final
/// text plus the parsed usage/cost the `result` line reported.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnResult {
    /// The agent reported success (subtype `success`, `is_error` false).
    pub(crate) ok: bool,
    /// The turn's final assistant message / `result` text.
    pub(crate) result: String,
    /// Token usage parsed from this turn's `result` line, when present.
    pub(crate) usage: Option<DelegateUsage>,
    /// Run cost in USD this turn's `result` line carried, when present.
    pub(crate) cost_usd: Option<f64>,
}

impl TurnResult {
    /// Render the turn as the JSON returned over the protocol (mirrors the DA-2
    /// one-shot [`DelegateOutcome`](crate::delegate_runtime::DelegateOutcome) shape
    /// so a steer result is shaped like a start result). `agent` is the catalog
    /// agent name (`claude` / `codex`) the turn ran under, so a steer result names
    /// the same agent the persistent driver speaks.
    #[must_use]
    pub(crate) fn to_json(&self, agent: &str, session_id: &str) -> Value {
        let usage = self.usage.map(|u| {
            json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "cache_read_tokens": u.cache_read_tokens,
                "cache_creation_tokens": u.cache_creation_tokens,
            })
        });
        json!({
            "agent": agent,
            "session_id": session_id,
            "ok": self.ok,
            "result": self.result,
            "usage": usage,
            "cost_usd": self.cost_usd,
        })
    }
}

/// A session-runtime failure distinct from a turn that merely reported an error
/// result: the child died, a turn stalled, or it was cancelled mid-flight. Shared
/// by both the claude (`stream-json`) and codex (`app-server`, DA-5c) drivers, so
/// the messages are agent-neutral ("delegated session …").
#[derive(Debug)]
pub(crate) enum SessionError {
    /// The child process exited (EOF on its stdout) before a turn completed.
    ProcessExited,
    /// A turn produced no `result` line within [`TURN_TIMEOUT`].
    TurnTimedOut,
    /// The turn was cancelled (the session's [`CancelToken`] fired).
    Cancelled,
    /// A lower-level spawn/IO failure.
    Io(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProcessExited => {
                write!(f, "delegated session exited before completing the turn")
            }
            Self::TurnTimedOut => write!(
                f,
                "delegated turn produced no result within the turn timeout"
            ),
            Self::Cancelled => write!(f, "delegated turn was cancelled"),
            Self::Io(message) => write!(f, "delegated session io error: {message}"),
        }
    }
}

/// A live, steerable delegated `claude` session: owns the persistent child, tracks
/// the `session_id` reported at init, and runs one turn per user message.
pub(crate) struct DelegateSession {
    child: PersistentChild,
    /// The claude `session_id` from the `system`/`init` line, captured on turn 1.
    session_id: Option<String>,
    /// DA-5b: when present, the child runs in proxied mode and `can_use_tool` asks
    /// are routed through this proxy to Nerve's approval hub. `None` keeps the DA-5a
    /// autonomy-driven mode (no approver to route to).
    proxy: Option<DelegateProxy>,
}

impl DelegateSession {
    /// Spawn the persistent `claude` child through the gated launcher, send the
    /// first user message, and run turn 1 — forwarding assistant text to
    /// `on_progress` and returning the turn's [`TurnResult`]. The launcher gate
    /// (a refusing launcher when `--allow-delegate` is off) is honored here: a
    /// refused spawn surfaces as [`SessionError::Io`].
    #[allow(clippy::too_many_arguments)] // reason: one cohesive spawn call; cwd,
    // autonomy, model, the first message, the optional approval proxy, and the
    // cancel/progress sinks are independent inputs to the start, and bundling them
    // into a struct would add indirection without isolating a separate responsibility.
    pub(crate) fn start(
        launcher: &dyn SandboxLauncher,
        cwd: &Path,
        autonomy: DelegateAutonomy,
        model: Option<&str>,
        first_message: &str,
        proxy: Option<DelegateProxy>,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<(Self, TurnResult), SessionError> {
        // Proxied mode (an approver is available) makes claude ASK before each tool;
        // the DA-5a autonomy mode is the no-approver fallback.
        let spec = if proxy.is_some() {
            build_proxied_claude_command(cwd, model)
        } else {
            build_persistent_claude_command(cwd, autonomy, model)
        };
        let policy = crate::delegate_runtime::delegate_policy(cwd);
        let child = launcher
            .launch_persistent(&spec, &policy)
            .map_err(|err| SessionError::Io(err.to_string()))?;
        let mut session = Self {
            child,
            session_id: None,
            proxy,
        };
        match session.run_turn(first_message, cancel, on_progress) {
            Ok(turn) => Ok((session, turn)),
            // A turn-1 failure (cancel mid-approval, stall, child death) must not
            // leak the child: reap it before returning, since the unregistered
            // session is about to be dropped (drop alone does not kill the child).
            Err(err) => {
                session.close();
                Err(err)
            }
        }
    }

    /// Steer the session with a follow-up user message, running one more turn.
    pub(crate) fn steer(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<TurnResult, SessionError> {
        self.run_turn(message, cancel, on_progress)
    }

    /// The claude session id captured at init, if the init line has been seen.
    #[must_use]
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Wrap an already-spawned [`PersistentChild`] without running a turn — used by
    /// tests that need a live `DelegateSession` over a fake child (e.g. to assert
    /// [`Self::close`] reaps the process group rather than leaking it).
    #[cfg(all(test, unix))]
    pub(crate) fn from_child_for_test(child: PersistentChild) -> Self {
        Self {
            child,
            session_id: None,
            proxy: None,
        }
    }

    /// The child's process-group id (its pid), for tests that assert the child is
    /// reaped (no leaked process) after [`Self::close`].
    #[cfg(all(test, unix))]
    pub(crate) fn child_pid(&self) -> u32 {
        self.child.pid()
    }

    /// Write one user message as a single framed stream-json line, then drive the
    /// read loop until this turn's `result` (forwarding assistant text). Captures
    /// the `session_id` from any `init` line seen along the way.
    fn run_turn(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<TurnResult, SessionError> {
        let line = format!("{}\n", user_message_frame(message));
        self.child
            .write_line(&line)
            .map_err(|err| SessionError::Io(err.to_string()))?;
        self.read_until_result(cancel, on_progress)
    }

    /// Block on the child's stdout line stream until a `result` line for the
    /// in-flight turn arrives, forwarding assistant text to `on_progress`. Honors
    /// `cancel` (sends an interrupt then reports [`SessionError::Cancelled`]),
    /// child exit, and the per-turn timeout.
    fn read_until_result(
        &mut self,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<TurnResult, SessionError> {
        let mut acc = TurnAccumulator::default();
        let deadline = std::time::Instant::now() + TURN_TIMEOUT;
        loop {
            if cancel.is_cancelled() {
                self.interrupt();
                return Err(SessionError::Cancelled);
            }
            match self.child.lines().recv_timeout(POLL_INTERVAL) {
                Ok(raw) => match self.ingest_line(&raw, &mut acc, cancel, on_progress) {
                    LineOutcome::Done(turn) => return Ok(turn),
                    LineOutcome::Interrupted => return Err(SessionError::Cancelled),
                    LineOutcome::Continue => {}
                },
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        return Err(SessionError::TurnTimedOut);
                    }
                }
                // The reader thread ended: the child closed stdout (exited).
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(SessionError::ProcessExited);
                }
            }
        }
    }

    /// Parse one NDJSON line: capture `session_id` from `init`, forward
    /// `assistant` text as progress, route a `can_use_tool` control_request through
    /// the approval proxy (DA-5b), and finish the turn on the `result` line.
    /// Non-JSON / envelope lines are ignored.
    fn ingest_line(
        &mut self,
        raw: &str,
        acc: &mut TurnAccumulator,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> LineOutcome {
        let line = reescape_control_chars(raw);
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            return LineOutcome::Continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("system") => {
                if value.get("subtype").and_then(Value::as_str) == Some("init")
                    && let Some(id) = value.get("session_id").and_then(Value::as_str)
                {
                    self.session_id.get_or_insert_with(|| id.to_string());
                }
                LineOutcome::Continue
            }
            Some("assistant") => {
                if let Some(text) = assistant_text(&value) {
                    on_progress(&text);
                    acc.last_assistant = text;
                }
                LineOutcome::Continue
            }
            Some("control_request") => self.handle_control_request(&value, cancel),
            Some("result") => {
                if let Some(id) = value.get("session_id").and_then(Value::as_str) {
                    self.session_id.get_or_insert_with(|| id.to_string());
                }
                LineOutcome::Done(acc.finish(&value))
            }
            // `user` tool-result echoes and `keep_alive` envelopes are ignored.
            _ => LineOutcome::Continue,
        }
    }

    /// Handle a `control_request` line. A `can_use_tool` ask is resolved through the
    /// proxy (blocking the reader for the operator decision — the claude turn is
    /// itself blocked on the response) and answered with a `control_response`; a
    /// deny+interrupt also ends the turn as cancelled. Without a proxy (DA-5a) the
    /// ask is logged-and-ignored (claude is in autonomy mode and won't actually ask).
    fn handle_control_request(&self, value: &Value, cancel: &CancelToken) -> LineOutcome {
        let subtype = value
            .get("request")
            .and_then(|r| r.get("subtype"))
            .and_then(Value::as_str);
        if subtype != Some("can_use_tool") {
            return LineOutcome::Continue;
        }
        let Some(proxy) = self.proxy.as_ref() else {
            return LineOutcome::Continue;
        };
        let response = proxy.resolve(value, cancel);
        // Best-effort write: a broken pipe means the child is already gone, which
        // the next recv on the line stream surfaces as ProcessExited.
        let _ = self.child.write_line(&format!("{}\n", response.line));
        if response.interrupted {
            self.interrupt();
            return LineOutcome::Interrupted;
        }
        LineOutcome::Continue
    }

    /// Send an `interrupt` control request to abort the in-flight turn (best
    /// effort — a broken pipe means the child is already gone).
    pub(crate) fn interrupt(&self) {
        let frame = json!({
            "type": "control_request",
            "request_id": new_request_id(),
            "request": { "subtype": "interrupt", "reason": "cancelled" },
        });
        let _ = self.child.write_line(&format!("{frame}\n"));
    }

    /// End the session: close stdin (EOF → graceful exit) and reap. If the child
    /// does not exit promptly after EOF, force-kill its process group.
    pub(crate) fn close(&mut self) {
        self.child.close_stdin();
        // Give the child a brief window to exit on EOF; then force the group down
        // so close() never blocks indefinitely on a child that ignores EOF.
        for _ in 0..50 {
            if self.child.has_exited() {
                break;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        if !self.child.has_exited() {
            self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// What one parsed stdout line means for the in-flight turn loop: keep reading,
/// the turn finished (with its result), or the turn was interrupted (a proxied
/// deny+interrupt) and should end as cancelled.
enum LineOutcome {
    Continue,
    Done(TurnResult),
    Interrupted,
}

/// Per-turn accumulator: the latest assistant text (the streamed answer) folded
/// with whatever the `result` line carries.
#[derive(Default)]
struct TurnAccumulator {
    last_assistant: String,
}

impl TurnAccumulator {
    /// Build the [`TurnResult`] from the `result` line, preferring its `result`
    /// text but falling back to the last streamed assistant text.
    fn finish(&self, value: &Value) -> TurnResult {
        let ok = value.get("subtype").and_then(Value::as_str) == Some("success")
            && !value
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let result = value
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.last_assistant.clone());
        let usage = value.get("usage").map(parse_usage);
        let cost_usd = value.get("total_cost_usd").and_then(Value::as_f64);
        TurnResult {
            ok,
            result,
            usage,
            cost_usd,
        }
    }
}

/// Concatenate the `text` of a claude `assistant` message's content blocks.
fn assistant_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let text: String = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

/// Parse a claude `result.usage` object into [`DelegateUsage`] (same field names
/// as the DA-2 one-shot parser).
fn parse_usage(usage: &Value) -> DelegateUsage {
    let get = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    DelegateUsage {
        input_tokens: get("input_tokens"),
        output_tokens: get("output_tokens"),
        cache_read_tokens: get("cache_read_input_tokens"),
        cache_creation_tokens: get("cache_creation_input_tokens"),
    }
}

/// Build the persistent `claude` argv (stream-json **in and out**, kept alive on
/// an open stdin). Mirrors the DA-2 one-shot recipe but adds `--input-format
/// stream-json` (so the process reads framed user messages over stdin) and omits
/// the trailing prompt.
fn build_persistent_claude_command(
    cwd: &Path,
    autonomy: DelegateAutonomy,
    model: Option<&str>,
) -> CommandSpec {
    let permission_mode = match autonomy {
        DelegateAutonomy::ReadOnly => "plan",
        DelegateAutonomy::Edit => "acceptEdits",
        DelegateAutonomy::Full => "bypassPermissions",
    };
    let mut args = vec![
        "-p".to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--permission-mode".to_string(),
        permission_mode.to_string(),
    ];
    if let Some(model) = model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("--add-dir".to_string());
    args.push(cwd.display().to_string());
    CommandSpec {
        command: "claude".to_string(),
        args,
    }
}

/// Build the **proxied-mode** persistent `claude` argv (DA-5b): like
/// [`build_persistent_claude_command`] but with `--permission-prompt-tool stdio
/// --permission-mode default` instead of the autonomy→permission-mode mapping, so
/// claude asks (via stdout `can_use_tool` control_requests) before each tool use
/// and Nerve approves. The autonomy argument is intentionally dropped here: in
/// proxied mode the operator's approval — not a fixed permission mode — governs
/// every tool call.
fn build_proxied_claude_command(cwd: &Path, model: Option<&str>) -> CommandSpec {
    let mut args = vec![
        "-p".to_string(),
        "--verbose".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--permission-prompt-tool".to_string(),
        "stdio".to_string(),
        "--permission-mode".to_string(),
        "default".to_string(),
    ];
    if let Some(model) = model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("--add-dir".to_string());
    args.push(cwd.display().to_string());
    CommandSpec {
        command: "claude".to_string(),
        args,
    }
}

/// Frame one user message as the claude stream-json input shape (the body only;
/// the caller appends the trailing `\n` and writes it as one flush).
fn user_message_frame(message: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": message }] },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// Re-escape raw control characters (<0x20, except the line was already split on
/// `\n`) inside a stdout line so a stray bare control byte in a string can't break
/// `serde_json` parsing. Tabs/CR are escaped; already-escaped sequences are left
/// alone since we only touch literal control bytes.
fn reescape_control_chars(line: &str) -> String {
    if !line.bytes().any(|b| b < 0x20) {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// A unique-enough control-request id without pulling in a uuid dependency: the
/// current nanosecond clock, which is monotonic-enough across the few interrupts
/// one session ever sends.
fn new_request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("interrupt-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistent_claude_argv_uses_stream_json_in_and_out() {
        let spec =
            build_persistent_claude_command(Path::new("/work"), DelegateAutonomy::ReadOnly, None);
        assert_eq!(spec.command, "claude");
        assert_eq!(
            spec.args,
            vec![
                "-p",
                "--verbose",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--permission-mode",
                "plan",
                "--add-dir",
                "/work",
            ]
        );
    }

    #[test]
    fn proxied_claude_argv_uses_permission_prompt_tool_and_default_mode() {
        // DA-5b: proxied mode swaps the autonomy→permission-mode mapping for the
        // stdio permission prompt + the `default` mode (so claude ASKS), and drops
        // the autonomy argument entirely.
        let spec = build_proxied_claude_command(Path::new("/work"), None);
        assert_eq!(spec.command, "claude");
        assert_eq!(
            spec.args,
            vec![
                "-p",
                "--verbose",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json",
                "--permission-prompt-tool",
                "stdio",
                "--permission-mode",
                "default",
                "--add-dir",
                "/work",
            ]
        );
        // No autonomy-derived mode leaks into proxied argv.
        assert!(!spec.args.iter().any(|a| a == "plan" || a == "acceptEdits"));
    }

    #[test]
    fn proxied_claude_argv_keeps_model() {
        let spec = build_proxied_claude_command(Path::new("/w"), Some("claude-sonnet-4-6"));
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["--model", "claude-sonnet-4-6"])
        );
        assert!(
            spec.args
                .windows(2)
                .any(|w| w == ["--permission-mode", "default"])
        );
    }

    #[test]
    fn persistent_claude_argv_maps_autonomy_and_model() {
        let edit = build_persistent_claude_command(
            Path::new("/w"),
            DelegateAutonomy::Edit,
            Some("claude-sonnet-4-6"),
        );
        assert!(edit.args.iter().any(|a| a == "acceptEdits"));
        assert!(
            edit.args
                .windows(2)
                .any(|w| w == ["--model", "claude-sonnet-4-6"])
        );

        let full = build_persistent_claude_command(Path::new("/w"), DelegateAutonomy::Full, None);
        assert!(full.args.iter().any(|a| a == "bypassPermissions"));
    }

    #[test]
    fn user_message_frame_is_valid_stream_json() {
        let frame = user_message_frame("hello \"world\"");
        let value: Value = serde_json::from_str(&frame).expect("valid json");
        assert_eq!(value["type"], "user");
        assert_eq!(value["message"]["role"], "user");
        assert_eq!(value["message"]["content"][0]["type"], "text");
        assert_eq!(value["message"]["content"][0]["text"], "hello \"world\"");
        assert!(value["parent_tool_use_id"].is_null());
    }

    #[test]
    fn turn_accumulator_prefers_result_text_then_falls_back() {
        let acc = TurnAccumulator {
            last_assistant: "streamed".to_string(),
        };
        let with_result = acc.finish(&json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "result": "final answer",
            "total_cost_usd": 0.02,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 1,
            },
        }));
        assert!(with_result.ok);
        assert_eq!(with_result.result, "final answer");
        assert_eq!(with_result.cost_usd, Some(0.02));
        assert_eq!(
            with_result.usage,
            Some(DelegateUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: 3,
                cache_creation_tokens: 1,
            })
        );

        // No `result` text → fall back to the streamed assistant text.
        let fallback = acc.finish(&json!({ "type": "result", "subtype": "success" }));
        assert_eq!(fallback.result, "streamed");
    }

    #[test]
    fn error_result_is_not_ok() {
        let acc = TurnAccumulator::default();
        let turn = acc.finish(&json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "result": "boom",
        }));
        assert!(!turn.ok);
        assert_eq!(turn.result, "boom");
    }

    #[test]
    fn reescape_leaves_clean_lines_untouched_and_escapes_control_bytes() {
        assert_eq!(reescape_control_chars("clean line"), "clean line");
        let dirty = "a\u{0001}b";
        assert_eq!(reescape_control_chars(dirty), "a\\u0001b");
    }

    #[test]
    fn turn_result_json_shape() {
        let turn = TurnResult {
            ok: true,
            result: "done".into(),
            usage: Some(DelegateUsage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            }),
            cost_usd: Some(0.1),
        };
        let json = turn.to_json("claude", "sess-1");
        assert_eq!(json["agent"], "claude");
        assert_eq!(json["session_id"], "sess-1");
        assert_eq!(json["ok"], true);
        assert_eq!(json["result"], "done");
        assert_eq!(json["usage"]["input_tokens"], 1);
        assert_eq!(json["cost_usd"], 0.1);
    }
}
