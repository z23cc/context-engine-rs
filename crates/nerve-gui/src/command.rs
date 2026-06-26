//! Typed constructors for the outbound runtime commands the GUI sends to the
//! daemon over `/rpc`.
//!
//! Every other call site builds a command as a free-form `json!({ "kind": … })`
//! literal handed to [`crate::rpc::start_job`] family. That drifts silently: a
//! renamed or typo'd command/field only fails at daemon *runtime*, never at GUI
//! *build* time. These constructors instead build the daemon's real typed
//! [`nerve_proto::RuntimeCommand`] enum and serialize it, so a renamed variant or
//! field is a **compile error** here. The protocol authority is shared (the GUI
//! already depends on `nerve-proto`), so there is no duplicated vocabulary.
//!
//! The output is a `serde_json::Value` that is byte-for-byte identical to the
//! previous literal (proven by the `#[cfg(test)]` equality assertions below), so
//! migrating a call site is purely a typing win — the wire is unchanged.
//!
//! Only the **static-kind** sites with a clean 1:1 typed variant live here.
//! Dynamic-kind sites (where `"kind"` is a runtime variable) and variants whose
//! `serde` shape would emit extra default fields (e.g. `wechat.login`'s
//! always-serialized `bot_type`) are intentionally left as literals.

use nerve_proto::{DelegateAutonomy, RuntimeCommand, SessionApprovalDecision};
use serde_json::Value;

/// Serialize a typed command to the wire `Value`. The enum derives `Serialize`
/// with `#[serde(tag = "kind")]`, so this yields `{ "kind": …, <fields> }` —
/// identical to the hand-written literals it replaces. Serialization of an owned
/// in-memory command cannot fail, so this never panics in practice.
fn to_value(command: &RuntimeCommand) -> Value {
    serde_json::to_value(command).expect("RuntimeCommand serializes to JSON")
}

/// `session.start` — provider/model are required; `workspace` is omitted from the
/// wire when empty (matching the literal's conditional insert and the variant's
/// `skip_serializing_if`).
pub(crate) fn session_start(provider: &str, model: &str, workspace: &str) -> Value {
    to_value(&RuntimeCommand::SessionStart {
        workspace: optional(workspace),
        provider: provider.to_string(),
        model: model.to_string(),
        system_prompt: None,
        agent: None,
        resume: None,
        max_turns: None,
        temperature: None,
        reasoning_effort: None,
        tool_filter: None,
    })
}

/// `session.message`.
pub(crate) fn session_message(session_id: &str, text: &str) -> Value {
    to_value(&RuntimeCommand::SessionMessage {
        session_id: session_id.to_string(),
        text: text.to_string(),
    })
}

/// `session.close`.
pub(crate) fn session_close(session_id: &str) -> Value {
    to_value(&RuntimeCommand::SessionClose {
        session_id: session_id.to_string(),
    })
}

/// `session.interrupt`.
pub(crate) fn session_interrupt(session_id: &str) -> Value {
    to_value(&RuntimeCommand::SessionInterrupt {
        session_id: session_id.to_string(),
    })
}

/// `session.respond`. `decision` is the snake_case wire token the buttons emit
/// (`allow` / `deny` / `allow_always` / `deny_always`); it is parsed into the
/// typed [`nerve_proto::SessionApprovalDecision`] so an unknown token is caught
/// here rather than silently shipped.
pub(crate) fn session_respond(session_id: &str, request_id: &str, decision: &str) -> Value {
    to_value(&RuntimeCommand::SessionRespond {
        session_id: session_id.to_string(),
        request_id: request_id.to_string(),
        decision: parse_decision(decision),
    })
}

/// `delegate.start`. `autonomy` is the snake_case wire token (`read_only` /
/// `edit` / `full`) the composer emits. `workspace`/`model`/`cwd` are omitted
/// from the wire when empty (matching the literal's conditional inserts).
pub(crate) fn delegate_start(
    agent: &str,
    task: &str,
    autonomy: &str,
    workspace: &str,
    model: &str,
    cwd: &str,
) -> Value {
    to_value(&RuntimeCommand::DelegateStart {
        agent: agent.to_string(),
        task: task.to_string(),
        workspace: optional(workspace),
        cwd: optional(cwd),
        autonomy: parse_autonomy(autonomy),
        role: Default::default(),
        model: optional(model),
        mcp_enable: None,
    })
}

/// `delegate.steer`.
pub(crate) fn delegate_steer(session_id: &str, message: &str) -> Value {
    to_value(&RuntimeCommand::DelegateSteer {
        session_id: session_id.to_string(),
        message: message.to_string(),
    })
}

/// `tool.call`. `arguments` must be a JSON object (the only shape the GUI ever
/// passes); it is carried through unchanged.
pub(crate) fn tool_call(name: &str, arguments: Value) -> Value {
    let arguments = arguments
        .as_object()
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    to_value(&RuntimeCommand::ToolCall {
        name: name.to_string(),
        arguments,
    })
}

/// `host.capabilities`.
pub(crate) fn host_capabilities() -> Value {
    to_value(&RuntimeCommand::HostCapabilities)
}

/// `ledger.verify` — re-derive the append-only evidence ledger's hash chain and
/// report whether it is intact (read-only; the tamper-detection moat). A unit
/// variant, so the wire is just `{ "kind": "ledger.verify" }`.
pub(crate) fn ledger_verify() -> Value {
    to_value(&RuntimeCommand::LedgerVerify)
}

/// `host.clipboard.write_text`.
pub(crate) fn host_clipboard_write_text(text: String) -> Value {
    to_value(&RuntimeCommand::HostClipboardWriteText { text })
}

/// `host.notification.show`.
pub(crate) fn host_notification_show(title: &str, body: &str) -> Value {
    to_value(&RuntimeCommand::HostNotificationShow {
        title: title.to_string(),
        body: Some(body.to_string()),
    })
}

/// `host.folder.pick`.
pub(crate) fn host_folder_pick(title: &str) -> Value {
    to_value(&RuntimeCommand::HostFolderPick {
        title: Some(title.to_string()),
    })
}

/// `host.file.save_text`.
pub(crate) fn host_file_save_text(title: &str, default_name: &str, text: String) -> Value {
    to_value(&RuntimeCommand::HostFileSaveText {
        title: Some(title.to_string()),
        default_name: Some(default_name.to_string()),
        text,
    })
}

/// `host.url.open`.
pub(crate) fn host_url_open(url: &str) -> Value {
    to_value(&RuntimeCommand::HostUrlOpen {
        url: url.to_string(),
    })
}

/// `workspace.reveal`. Unlike the other workspace-routed commands, the original
/// literal ALWAYS emitted `"workspace": <ws>` (even when empty), so this passes the
/// value through verbatim — `Some("")` stays on the wire — rather than omitting it.
pub(crate) fn workspace_reveal(workspace: &str) -> Value {
    to_value(&RuntimeCommand::WorkspaceReveal {
        workspace: Some(workspace.to_string()),
    })
}

/// `wechat.start`. `owners`/`agent`/`autonomy` are always serialized (no
/// `skip_serializing_if` on the variant), matching the literal's three fields.
pub(crate) fn wechat_start(owners: Vec<String>, agent: &str, autonomy: &str) -> Value {
    to_value(&RuntimeCommand::WechatStart {
        owners,
        agent: agent.to_string(),
        autonomy: parse_autonomy(autonomy),
    })
}

/// `None` for an empty string, mirroring the `if !x.is_empty()` conditional
/// inserts the literals used (and the variants' `skip_serializing_if`).
fn optional(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// Parse an autonomy token; defaults to the most-restricted posture
/// ([`DelegateAutonomy`]'s own `#[default]`) on an unknown value, so a stray UI
/// value can never panic the frontend.
fn parse_autonomy(token: &str) -> DelegateAutonomy {
    serde_json::from_value(Value::String(token.to_string())).unwrap_or_default()
}

/// Parse an approval-decision token (`allow` / `deny` / `allow_always` /
/// `deny_always`). [`SessionApprovalDecision`] has no `Default`, so an unknown
/// token falls back to the safe-by-default `Deny` rather than panicking.
fn parse_decision(token: &str) -> SessionApprovalDecision {
    serde_json::from_value(Value::String(token.to_string()))
        .unwrap_or(SessionApprovalDecision::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Each assertion locks a constructor's emitted JSON to the EXACT literal that
    // previously lived at the call site, proving the migration is byte-identical
    // (value-equal; `serde_json::Value` map equality is order-independent).

    #[test]
    fn session_start_matches_literal_with_workspace() {
        assert_eq!(
            session_start("anthropic", "claude", "proj"),
            json!({ "kind": "session.start", "provider": "anthropic", "model": "claude", "workspace": "proj" })
        );
    }

    #[test]
    fn session_start_matches_literal_without_workspace() {
        assert_eq!(
            session_start("anthropic", "claude", ""),
            json!({ "kind": "session.start", "provider": "anthropic", "model": "claude" })
        );
    }

    #[test]
    fn session_message_matches_literal() {
        assert_eq!(
            session_message("s1", "hi"),
            json!({ "kind": "session.message", "session_id": "s1", "text": "hi" })
        );
    }

    #[test]
    fn session_close_matches_literal() {
        assert_eq!(
            session_close("s1"),
            json!({ "kind": "session.close", "session_id": "s1" })
        );
    }

    #[test]
    fn session_interrupt_matches_literal() {
        assert_eq!(
            session_interrupt("s1"),
            json!({ "kind": "session.interrupt", "session_id": "s1" })
        );
    }

    #[test]
    fn session_respond_matches_literal() {
        for decision in ["allow", "deny", "allow_always", "deny_always"] {
            assert_eq!(
                session_respond("s1", "r1", decision),
                json!({ "kind": "session.respond", "session_id": "s1", "request_id": "r1", "decision": decision })
            );
        }
    }

    #[test]
    fn delegate_start_matches_literal_full() {
        // All optionals present — mirrors the literal after every conditional fired.
        assert_eq!(
            delegate_start("codex", "do it", "full", "proj", "gpt", "/root"),
            json!({
                "kind": "delegate.start",
                "agent": "codex",
                "task": "do it",
                "autonomy": "full",
                "workspace": "proj",
                "model": "gpt",
                "cwd": "/root",
            })
        );
    }

    #[test]
    fn delegate_start_matches_literal_minimal() {
        // No workspace/model/cwd — mirrors the literal with no conditional fired.
        assert_eq!(
            delegate_start("claude", "explore", "read_only", "", "", ""),
            json!({
                "kind": "delegate.start",
                "agent": "claude",
                "task": "explore",
                "autonomy": "read_only",
            })
        );
    }

    #[test]
    fn delegate_steer_matches_literal() {
        assert_eq!(
            delegate_steer("job1", "and now this"),
            json!({ "kind": "delegate.steer", "session_id": "job1", "message": "and now this" })
        );
    }

    #[test]
    fn tool_call_matches_literal() {
        let args = json!({ "query": "x", "limit": 5 });
        assert_eq!(
            tool_call("scout", args.clone()),
            json!({ "kind": "tool.call", "name": "scout", "arguments": args })
        );
    }

    #[test]
    fn host_capabilities_matches_literal() {
        assert_eq!(host_capabilities(), json!({ "kind": "host.capabilities" }));
    }

    #[test]
    fn ledger_verify_matches_literal() {
        assert_eq!(ledger_verify(), json!({ "kind": "ledger.verify" }));
    }

    #[test]
    fn host_clipboard_write_text_matches_literal() {
        assert_eq!(
            host_clipboard_write_text("copied".into()),
            json!({ "kind": "host.clipboard.write_text", "text": "copied" })
        );
    }

    #[test]
    fn host_notification_show_matches_literal() {
        assert_eq!(
            host_notification_show("Nerve", "Host OS notifications are available."),
            json!({
                "kind": "host.notification.show",
                "title": "Nerve",
                "body": "Host OS notifications are available.",
            })
        );
    }

    #[test]
    fn host_folder_pick_matches_literal() {
        assert_eq!(
            host_folder_pick("Pick a folder"),
            json!({ "kind": "host.folder.pick", "title": "Pick a folder" })
        );
    }

    #[test]
    fn host_file_save_text_matches_literal() {
        assert_eq!(
            host_file_save_text("Save", "out.txt", "body".into()),
            json!({
                "kind": "host.file.save_text",
                "title": "Save",
                "default_name": "out.txt",
                "text": "body",
            })
        );
    }

    #[test]
    fn host_url_open_matches_literal() {
        assert_eq!(
            host_url_open("https://example.com"),
            json!({ "kind": "host.url.open", "url": "https://example.com" })
        );
    }

    #[test]
    fn workspace_reveal_matches_literal() {
        assert_eq!(
            workspace_reveal("proj"),
            json!({ "kind": "workspace.reveal", "workspace": "proj" })
        );
        // The literal always emitted the field, even when empty — preserve that.
        assert_eq!(
            workspace_reveal(""),
            json!({ "kind": "workspace.reveal", "workspace": "" })
        );
    }

    #[test]
    fn wechat_start_matches_literal() {
        let owners = vec!["wxid_a".to_string(), "wxid_b".to_string()];
        assert_eq!(
            wechat_start(owners.clone(), "claude", "read_only"),
            json!({
                "kind": "wechat.start",
                "owners": owners,
                "agent": "claude",
                "autonomy": "read_only",
            })
        );
    }
}
