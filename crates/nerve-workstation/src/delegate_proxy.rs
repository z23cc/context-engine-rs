//! DA-5b: routing a delegated `claude` session's tool-permission prompts through
//! Nerve's own approval system.
//!
//! [`delegate_session`](crate::delegate_session) (DA-5a) runs a persistent claude
//! child driven by `--permission-mode <plan|acceptEdits|bypassPermissions>`, where
//! claude decides tool permissions itself. This module adds **proxied mode**: when
//! an [`ApprovalHub`](crate::session_manager::ApprovalHub)-backed approver is
//! available, the child is started with `--permission-prompt-tool stdio
//! --permission-mode default`, so claude *asks before each tool use* and Nerve's
//! operator approves (via the same approval modal that gates Nerve's own tools).
//!
//! ## The pinned claude permission protocol (verified, live)
//!
//! When a tool needs approval, claude emits on stdout (the turn blocks until a
//! response is written back):
//! ```json
//! {"type":"control_request","request_id":"<uuid>","request":{
//!     "subtype":"can_use_tool","tool_name":"Bash","input":{...},
//!     "permission_suggestions":[...],"tool_use_id":"toolu_...","blocked_path":"..."}}
//! ```
//! The reply is one framed write — **outer** envelope snake_case, **inner**
//! decision camelCase:
//! ```json
//! ALLOW: {"type":"control_response","response":{"subtype":"success",
//!   "request_id":"<same>","response":{"behavior":"allow",
//!   "updatedInput":<echo input>,"toolUseID":"<tool_use_id>"}}}
//! DENY:  {"type":"control_response","response":{"subtype":"success",
//!   "request_id":"<same>","response":{"behavior":"deny","message":"<reason>"}}}
//! ```
//! A deny that also cancels the whole turn adds `"interrupt":true` to the deny
//! inner object. A `control_cancel_request` (claude withdrew a pending ask) drops
//! the pending approval; a `keep_alive` is ignored.

use nerve_core::CancelToken;
use nerve_runtime::{RiskTier, SessionApprovalDecision};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-delegate-session memory of remembered approval decisions: tool name →
/// `true` (allow-always) / `false` (deny-always). Mirrors the session agent's
/// `DecisionMemory` so an `AllowAlways` / `DenyAlways` persists across `can_use_tool`
/// asks for the life of the delegated session, and a deny-always never re-prompts
/// (a claude that keeps re-requesting a refused tool can't wedge the operator).
pub(crate) type DelegateDecisions = Arc<Mutex<HashMap<String, bool>>>;

/// The approval seam the delegated session calls into. Implemented by the
/// session-manager's `ApprovalHub`, so a `can_use_tool` ask emits an
/// `approval_requested` event (the TUI modal) and blocks for the operator's
/// `session.respond` — the exact round-trip Nerve's own tools use. Kept as a trait
/// so [`delegate_session`](crate::delegate_session) does not depend on
/// session-manager internals.
pub(crate) trait DelegateApprover: Send + Sync {
    /// Emit an `approval_requested` for `tool` (with `args`, `tier`, `preview`)
    /// under `session_id` and block for the operator's decision. Cancellation /
    /// timeout resolve to [`SessionApprovalDecision::Deny`].
    fn request(
        &self,
        session_id: &str,
        tool: &str,
        args: &Value,
        tier: RiskTier,
        preview: String,
        cancel: &CancelToken,
    ) -> SessionApprovalDecision;
}

/// Proxied-mode permission state for a live delegated session: the approver to
/// route `can_use_tool` asks to, the delegate session id (== the start job id) the
/// approval is keyed under, and the per-session remembered allow/deny decisions.
pub(crate) struct DelegateProxy {
    approver: Arc<dyn DelegateApprover>,
    session_id: String,
    decisions: DelegateDecisions,
}

/// What the reader should do after handling a `can_use_tool` ask: write the built
/// `control_response` line, and (on a deny+interrupt) treat the turn as cancelled.
pub(crate) struct ProxyResponse {
    /// The framed `control_response` line (no trailing newline) to write on stdin.
    pub(crate) line: String,
    /// Whether the response interrupted the turn (deny while the session's cancel
    /// token fired) — the caller writes the line then ends the turn as cancelled.
    pub(crate) interrupted: bool,
}

impl DelegateProxy {
    pub(crate) fn new(
        approver: Arc<dyn DelegateApprover>,
        session_id: String,
        decisions: DelegateDecisions,
    ) -> Self {
        Self {
            approver,
            session_id,
            decisions,
        }
    }

    /// Resolve a `can_use_tool` control_request into a `control_response` line.
    ///
    /// A remembered decision short-circuits without a fresh prompt (allow-always →
    /// allow, deny-always → deny). Otherwise the approval hub is consulted (which
    /// blocks the reader thread — acceptable: the claude turn is itself blocked on
    /// the response). A remembered allow/deny is recorded so repeats skip the
    /// prompt; a one-shot Allow/Deny applies to this call only. A deny that is
    /// really a cancel (the session token fired) interrupts the whole turn.
    pub(crate) fn resolve(&self, request: &Value, cancel: &CancelToken) -> ProxyResponse {
        let request_id = string_field(request, "request_id");
        let inner = request.get("request");
        let tool = inner
            .and_then(|r| r.get("tool_name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let input = inner
            .and_then(|r| r.get("input"))
            .cloned()
            .unwrap_or(Value::Null);
        let tool_use_id = inner
            .and_then(|r| r.get("tool_use_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let decision = self.decide(&tool, &input, cancel);
        if decision.allows() {
            return ProxyResponse {
                line: allow_response(&request_id, &input, &tool_use_id),
                interrupted: false,
            };
        }
        // A deny issued because the session was cancelled also interrupts the turn,
        // so claude tears the turn down rather than continuing after the refusal.
        let interrupt = cancel.is_cancelled();
        ProxyResponse {
            line: deny_response(&request_id, deny_message(&decision), interrupt),
            interrupted: interrupt,
        }
    }

    /// Decide the approval for `tool`: a remembered decision wins; otherwise prompt
    /// the operator and record an `AllowAlways` / `DenyAlways` for future asks.
    fn decide(&self, tool: &str, input: &Value, cancel: &CancelToken) -> SessionApprovalDecision {
        if let Some(&allow) = crate::sync::lock_recover(&self.decisions).get(tool) {
            return if allow {
                SessionApprovalDecision::Allow
            } else {
                SessionApprovalDecision::Deny
            };
        }
        let tier = claude_tool_tier(tool);
        let preview = delegate_preview(tool, input);
        let decision = self
            .approver
            .request(&self.session_id, tool, input, tier, preview, cancel);
        // Persist a remembered allow/deny so future asks for this tool skip the
        // prompt; don't persist a deny that is really a turn interrupt.
        if decision.remember() && !cancel.is_cancelled() {
            crate::sync::lock_recover(&self.decisions).insert(tool.to_string(), decision.allows());
        }
        decision
    }
}

/// Risk tier for a claude tool name. claude's tool vocabulary differs from Nerve's
/// (`Bash`/`Edit`/`Read`/…), so it is classified here rather than through the
/// Nerve-keyed [`crate::policy::tool_tier`]. Fail-safe: an unknown tool (a plugin
/// or a newly added one) classifies as [`RiskTier::Exec`], the most restricted
/// tier, so it is gated rather than silently treated as benign.
fn claude_tool_tier(tool: &str) -> RiskTier {
    match tool {
        // Reads / navigation — no mutation, no exec.
        "Read" | "Glob" | "Grep" | "NotebookRead" | "TodoWrite" | "WebFetch" | "WebSearch" => {
            RiskTier::ReadOnly
        }
        // File mutation within the workspace.
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => RiskTier::Edit,
        // Command execution and everything unrecognised (fail-safe).
        _ => RiskTier::Exec,
    }
}

/// The salient argument for a claude tool call, surfaced in the approval preview:
/// the command for `Bash`, the path for the file tools, the pattern/query for the
/// search tools, the URL for `WebFetch`. Falls back to a compact JSON dump.
fn claude_tool_summary(tool: &str, input: &Value) -> String {
    let field = match tool {
        "Bash" => "command",
        "Edit" | "Write" | "Read" | "NotebookEdit" | "MultiEdit" => "file_path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        _ => "",
    };
    if let Some(value) = input.get(field).and_then(Value::as_str) {
        return value.to_string();
    }
    if input.is_null() {
        String::new()
    } else {
        input.to_string()
    }
}

/// Delegate-aware approval preview: "claude wants to run <tool>: <summary>", where
/// the summary is the tool's salient argument (the command / path / query), so the
/// modal reads naturally for a delegated tool call rather than a raw JSON dump.
fn delegate_preview(tool: &str, input: &Value) -> String {
    const MAX: usize = 500;
    let summary = claude_tool_summary(tool, input);
    let rendered = if summary.is_empty() {
        format!("claude wants to run {tool}")
    } else {
        format!("claude wants to run {tool}: {summary}")
    };
    truncate_chars(&rendered, MAX)
}

/// Truncate to at most `max` characters, appending an ellipsis when cut (mirrors
/// [`crate::policy`]'s preview truncation so a long command can't bloat the modal).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}\u{2026}")
    }
}

/// The human-readable deny reason claude surfaces to its model. A `DenyAlways`
/// notes the standing refusal so the model stops re-requesting the tool.
fn deny_message(decision: &SessionApprovalDecision) -> &'static str {
    match decision {
        SessionApprovalDecision::DenyAlways => {
            "denied by the operator (this tool is blocked for the session)"
        }
        _ => "denied by the operator",
    }
}

/// Build the `allow` control_response: outer envelope snake_case, inner decision
/// camelCase. `updatedInput` echoes claude's requested input verbatim (Nerve does
/// not rewrite tool inputs); `toolUseID` echoes the request's `tool_use_id`.
pub(crate) fn allow_response(request_id: &str, input: &Value, tool_use_id: &str) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": input,
                "toolUseID": tool_use_id,
            },
        },
    })
    .to_string()
}

/// Build the `deny` control_response. With `interrupt` set, the inner object also
/// carries `"interrupt":true` so claude cancels the whole turn rather than letting
/// the model continue after the refusal.
pub(crate) fn deny_response(request_id: &str, message: &str, interrupt: bool) -> String {
    let mut inner = json!({ "behavior": "deny", "message": message });
    if interrupt {
        inner["interrupt"] = json!(true);
    }
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": inner,
        },
    })
    .to_string()
}

/// Read a string-valued top-level `field` from `value`, or `""` if absent.
fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_response_echoes_input_and_tool_use_id() {
        let input = json!({ "command": "ls -la" });
        let line = allow_response("req-1", &input, "toolu_abc");
        let value: Value = serde_json::from_str(&line).expect("valid json");
        // Outer envelope is snake_case.
        assert_eq!(value["type"], "control_response");
        assert_eq!(value["response"]["subtype"], "success");
        assert_eq!(value["response"]["request_id"], "req-1");
        // Inner decision is camelCase.
        let inner = &value["response"]["response"];
        assert_eq!(inner["behavior"], "allow");
        assert_eq!(inner["updatedInput"], input);
        assert_eq!(inner["toolUseID"], "toolu_abc");
    }

    #[test]
    fn deny_response_without_interrupt_omits_the_flag() {
        let line = deny_response("req-2", "denied by the operator", false);
        let value: Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(value["type"], "control_response");
        assert_eq!(value["response"]["subtype"], "success");
        assert_eq!(value["response"]["request_id"], "req-2");
        let inner = &value["response"]["response"];
        assert_eq!(inner["behavior"], "deny");
        assert_eq!(inner["message"], "denied by the operator");
        assert!(inner.get("interrupt").is_none());
    }

    #[test]
    fn deny_response_with_interrupt_sets_the_flag() {
        let line = deny_response("req-3", "denied by the operator", true);
        let value: Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(value["response"]["response"]["interrupt"], true);
        assert_eq!(value["response"]["response"]["behavior"], "deny");
    }

    /// A scripted approver returning a fixed decision, standing in for the hub.
    struct FixedApprover(SessionApprovalDecision);

    impl DelegateApprover for FixedApprover {
        fn request(
            &self,
            _session_id: &str,
            _tool: &str,
            _args: &Value,
            _tier: RiskTier,
            _preview: String,
            _cancel: &CancelToken,
        ) -> SessionApprovalDecision {
            self.0
        }
    }

    fn can_use_tool(request_id: &str, tool: &str, input: Value, tool_use_id: &str) -> Value {
        json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": tool,
                "input": input,
                "tool_use_id": tool_use_id,
            },
        })
    }

    fn proxy(decision: SessionApprovalDecision) -> DelegateProxy {
        DelegateProxy::new(
            Arc::new(FixedApprover(decision)),
            "sess-1".to_string(),
            DelegateDecisions::default(),
        )
    }

    #[test]
    fn resolve_allow_builds_allow_response_echoing_request() {
        let proxy = proxy(SessionApprovalDecision::Allow);
        let request = can_use_tool("r1", "Bash", json!({ "command": "echo hi" }), "toolu_1");
        let resp = proxy.resolve(&request, &CancelToken::never());
        assert!(!resp.interrupted);
        let value: Value = serde_json::from_str(&resp.line).expect("json");
        let inner = &value["response"]["response"];
        assert_eq!(inner["behavior"], "allow");
        assert_eq!(inner["updatedInput"], json!({ "command": "echo hi" }));
        assert_eq!(inner["toolUseID"], "toolu_1");
        assert_eq!(value["response"]["request_id"], "r1");
    }

    #[test]
    fn resolve_deny_builds_deny_response_without_interrupt() {
        let proxy = proxy(SessionApprovalDecision::Deny);
        let request = can_use_tool("r2", "Edit", json!({ "path": "x" }), "toolu_2");
        let resp = proxy.resolve(&request, &CancelToken::never());
        assert!(!resp.interrupted);
        let value: Value = serde_json::from_str(&resp.line).expect("json");
        assert_eq!(value["response"]["response"]["behavior"], "deny");
        assert!(value["response"]["response"].get("interrupt").is_none());
    }

    #[test]
    fn deny_under_cancel_interrupts_the_turn() {
        let proxy = proxy(SessionApprovalDecision::Deny);
        let cancel = CancelToken::new();
        cancel.cancel();
        let request = can_use_tool("r3", "Bash", json!({}), "toolu_3");
        let resp = proxy.resolve(&request, &cancel);
        assert!(resp.interrupted);
        let value: Value = serde_json::from_str(&resp.line).expect("json");
        assert_eq!(value["response"]["response"]["interrupt"], true);
    }

    #[test]
    fn allow_always_is_remembered_and_skips_the_second_prompt() {
        // An approver that records how many times it was consulted.
        struct CountingApprover {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl DelegateApprover for CountingApprover {
            fn request(
                &self,
                _session_id: &str,
                _tool: &str,
                _args: &Value,
                _tier: RiskTier,
                _preview: String,
                _cancel: &CancelToken,
            ) -> SessionApprovalDecision {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                SessionApprovalDecision::AllowAlways
            }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proxy = DelegateProxy::new(
            Arc::new(CountingApprover {
                calls: Arc::clone(&calls),
            }),
            "sess-1".to_string(),
            DelegateDecisions::default(),
        );
        let cancel = CancelToken::never();
        let first = proxy.resolve(&can_use_tool("r1", "Bash", json!({}), "t1"), &cancel);
        let second = proxy.resolve(&can_use_tool("r2", "Bash", json!({}), "t2"), &cancel);
        // Both allowed, but the operator was only consulted once (the second was
        // served from the remembered allow-always).
        assert_eq!(
            serde_json::from_str::<Value>(&first.line).unwrap()["response"]["response"]["behavior"],
            "allow"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&second.line).unwrap()["response"]["response"]["behavior"],
            "allow"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn deny_always_is_remembered_and_auto_denies_repeats() {
        struct CountingApprover {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl DelegateApprover for CountingApprover {
            fn request(
                &self,
                _session_id: &str,
                _tool: &str,
                _args: &Value,
                _tier: RiskTier,
                _preview: String,
                _cancel: &CancelToken,
            ) -> SessionApprovalDecision {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                SessionApprovalDecision::DenyAlways
            }
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proxy = DelegateProxy::new(
            Arc::new(CountingApprover {
                calls: Arc::clone(&calls),
            }),
            "sess-1".to_string(),
            DelegateDecisions::default(),
        );
        let cancel = CancelToken::never();
        let first = proxy.resolve(&can_use_tool("r1", "Bash", json!({}), "t1"), &cancel);
        let second = proxy.resolve(&can_use_tool("r2", "Bash", json!({}), "t2"), &cancel);
        assert_eq!(
            serde_json::from_str::<Value>(&first.line).unwrap()["response"]["response"]["behavior"],
            "deny"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&second.line).unwrap()["response"]["response"]["behavior"],
            "deny"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn delegate_preview_reads_naturally() {
        assert_eq!(
            delegate_preview("Bash", &json!({ "command": "ls" })),
            "claude wants to run Bash: ls"
        );
        assert_eq!(
            delegate_preview("Edit", &json!({ "file_path": "src/main.rs" })),
            "claude wants to run Edit: src/main.rs"
        );
        // A tool with no salient field still names the tool.
        assert_eq!(
            delegate_preview("UnknownTool", &Value::Null),
            "claude wants to run UnknownTool"
        );
        // Overlong previews are truncated with an ellipsis.
        let long = "x".repeat(600);
        let preview = delegate_preview("Bash", &json!({ "command": long }));
        assert_eq!(preview.chars().count(), 501);
        assert!(preview.ends_with('\u{2026}'));
    }

    #[test]
    fn claude_tool_tier_classifies_and_fails_safe() {
        for tool in ["Read", "Glob", "Grep", "WebFetch", "WebSearch"] {
            assert_eq!(claude_tool_tier(tool), RiskTier::ReadOnly, "{tool}");
        }
        for tool in ["Edit", "Write", "NotebookEdit", "MultiEdit"] {
            assert_eq!(claude_tool_tier(tool), RiskTier::Edit, "{tool}");
        }
        // Bash and any unknown / plugin tool fail safe to the top tier.
        for tool in ["Bash", "Task", "mcp__x__y", "BrandNewTool"] {
            assert_eq!(claude_tool_tier(tool), RiskTier::Exec, "{tool}");
        }
    }
}
