//! L0 granularity (Wave 2): lift an agent's structured stream into tool-lifecycle
//! provenance events.
//!
//! A captured Run's tape carries the raw `Output` lines verbatim, but those are
//! opaque text. This module parses the *structured* tool calls the agent streams DO
//! expose — claude `tool_use` / `tool_result` content blocks, codex
//! `command_execution` / `file_change` items — into
//! [`ToolStarted`](nerve_core::provenance::EventKind::ToolStarted) /
//! [`ToolFinished`](nerve_core::provenance::EventKind::ToolFinished) events, so a
//! recorded Run indexes *which* tools ran, files were edited, and commands executed
//! (`trust-substrate.md` §3 L0). Pure (a function of the line alone) and above the
//! determinism boundary (INV-R2); the SHA-256-over-canonical-JSON it calls lives in
//! `nerve_core::provenance`.

use super::DelegateAgent;
use nerve_core::provenance::{EventKind, hash_canonical_json};
use serde_json::Value;

/// Max bytes of a tool `title` (the file path / command summary) recorded on a
/// tool-lifecycle event — bounds the index field; the full input stays verbatim in
/// the raw `Output` lines.
const MAX_TOOL_TITLE: usize = 200;

/// Lift the structured tool calls an agent's stream line carries into L0
/// tool-lifecycle [`EventKind`]s (`tool_started` / `tool_finished`), so a recorded
/// Run indexes *which* tools ran / files were edited / commands executed — not just
/// opaque `Output` text. `gemini` returns empty (its stream shape is unverified —
/// Output-only, the honest partial).
pub(crate) fn parse_tool_events(agent: DelegateAgent, value: &Value, turn: u64) -> Vec<EventKind> {
    match agent {
        DelegateAgent::Claude => parse_claude_tool_events(value, turn),
        DelegateAgent::Codex => parse_codex_tool_events(value, turn),
        DelegateAgent::Gemini => Vec::new(),
    }
}

/// claude: an `assistant` message's `tool_use` content blocks become `ToolStarted`;
/// a `user` message's `tool_result` blocks become `ToolFinished`.
fn parse_claude_tool_events(value: &Value, turn: u64) -> Vec<EventKind> {
    let Some(blocks) = value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let role = value.get("type").and_then(Value::as_str);
    let mut events = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") if role == Some("assistant") => {
                let tool = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                events.push(EventKind::ToolStarted {
                    turn,
                    title: claude_tool_title(&input),
                    args_hash: hash_canonical_json(&input),
                    tool,
                });
            }
            Some("tool_result") if role == Some("user") => {
                let ok = !block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let content = block.get("content").cloned().unwrap_or(Value::Null);
                events.push(EventKind::ToolFinished {
                    turn,
                    // tool_result carries no tool name (only `tool_use_id`); the
                    // matching name is on the prior tool_use line and the raw tape.
                    tool: "tool".to_string(),
                    ok,
                    title: None,
                    output_hash: hash_canonical_json(&content),
                });
            }
            _ => {}
        }
    }
    events
}

/// codex: a completed `command_execution` / `file_change` item becomes a
/// `ToolStarted` + `ToolFinished` pair (the one-shot stream reports the item only
/// once it has finished, so both bracket events are synthesized from it).
fn parse_codex_tool_events(value: &Value, turn: u64) -> Vec<EventKind> {
    if !matches!(
        value.get("type").and_then(Value::as_str),
        Some("item") | Some("item.completed")
    ) {
        return Vec::new();
    }
    let item = value.get("item").unwrap_or(value);
    let Some((tool, title, ok)) = codex_item_tool(item) else {
        return Vec::new();
    };
    let args_hash = hash_canonical_json(item);
    vec![
        EventKind::ToolStarted {
            turn,
            tool: tool.clone(),
            title: title.clone(),
            args_hash: args_hash.clone(),
        },
        EventKind::ToolFinished {
            turn,
            tool,
            ok,
            title,
            output_hash: args_hash,
        },
    ]
}

/// Classify a codex `item` as a tool call: `(tool, title, ok)` for a
/// `command_execution` (title = command, ok from exit code) or `file_change`
/// (title = path); `None` for any other item type (e.g. `agent_message`).
fn codex_item_tool(item: &Value) -> Option<(String, Option<String>, bool)> {
    match item.get("type").and_then(Value::as_str) {
        Some("command_execution") => {
            let title = item
                .get("command")
                .and_then(Value::as_str)
                .map(bounded_title);
            let ok = item
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_none_or(|code| code == 0);
            Some(("command_execution".to_string(), title, ok))
        }
        Some("file_change") => {
            let title = item
                .get("path")
                .or_else(|| item.get("file"))
                .and_then(Value::as_str)
                .map(bounded_title);
            Some(("file_change".to_string(), title, true))
        }
        _ => None,
    }
}

/// Best-effort human title for a claude tool: its file path, else its command,
/// bounded. `None` when the input carries neither.
fn claude_tool_title(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("command"))
        .and_then(Value::as_str)
        .map(bounded_title)
}

/// Truncate a title to [`MAX_TOOL_TITLE`] bytes on a char boundary.
fn bounded_title(text: &str) -> String {
    if text.len() <= MAX_TOOL_TITLE {
        return text.to_string();
    }
    let mut end = MAX_TOOL_TITLE;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_tool_use_lifts_to_tool_started_with_bounded_title() {
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "text", "text": "editing now" },
                { "type": "tool_use", "id": "tu1", "name": "Edit",
                  "input": { "file_path": "src/lib.rs", "old_string": "a", "new_string": "b" } },
            ] }
        });
        let events = parse_tool_events(DelegateAgent::Claude, &line, 0);
        assert_eq!(events.len(), 1, "only the tool_use block lifts");
        match &events[0] {
            EventKind::ToolStarted {
                turn,
                tool,
                title,
                args_hash,
            } => {
                assert_eq!(*turn, 0);
                assert_eq!(tool, "Edit");
                assert_eq!(title.as_deref(), Some("src/lib.rs"));
                assert_eq!(args_hash.len(), 64, "args_hash is a SHA-256 hex digest");
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[test]
    fn claude_tool_result_lifts_to_tool_finished() {
        let ok_line = serde_json::json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": "tu1", "content": "edited", "is_error": false },
            ] }
        });
        let events = parse_tool_events(DelegateAgent::Claude, &ok_line, 1);
        assert!(
            matches!(&events[..], [EventKind::ToolFinished { turn: 1, ok: true, output_hash, .. }] if output_hash.len() == 64),
            "{events:?}"
        );
        // An errored tool_result flips ok to false.
        let err_line = serde_json::json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": "tu1", "content": "boom", "is_error": true },
            ] }
        });
        let events = parse_tool_events(DelegateAgent::Claude, &err_line, 1);
        assert!(
            matches!(&events[..], [EventKind::ToolFinished { ok: false, .. }]),
            "{events:?}"
        );
    }

    #[test]
    fn codex_command_execution_lifts_to_started_and_finished_pair() {
        let line = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "command": "cargo test", "exit_code": 0 }
        });
        let events = parse_tool_events(DelegateAgent::Codex, &line, 0);
        assert!(
            matches!(
                &events[..],
                [
                    EventKind::ToolStarted { tool: t1, title: Some(c1), .. },
                    EventKind::ToolFinished { tool: t2, ok: true, .. },
                ] if t1 == "command_execution" && c1 == "cargo test" && t2 == "command_execution"
            ),
            "{events:?}"
        );
        // A non-zero exit marks the finished event not-ok.
        let failed = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "command": "false", "exit_code": 1 }
        });
        let events = parse_tool_events(DelegateAgent::Codex, &failed, 0);
        assert!(
            matches!(
                &events[..],
                [
                    EventKind::ToolStarted { .. },
                    EventKind::ToolFinished { ok: false, .. }
                ]
            ),
            "{events:?}"
        );
    }

    #[test]
    fn codex_file_change_lifts_with_path_title() {
        let line = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "file_change", "path": "src/main.rs" }
        });
        let events = parse_tool_events(DelegateAgent::Codex, &line, 2);
        assert!(
            matches!(
                &events[..],
                [
                    EventKind::ToolStarted { turn: 2, tool, title: Some(p), .. },
                    EventKind::ToolFinished { turn: 2, .. },
                ] if tool == "file_change" && p == "src/main.rs"
            ),
            "{events:?}"
        );
    }

    #[test]
    fn gemini_and_plain_lines_yield_no_tool_events() {
        // Gemini is Output-only (unverified stream shape) -> never lifts tool events.
        let line = serde_json::json!({ "type": "assistant",
            "message": { "content": [{ "type": "tool_use", "name": "X", "input": {} }] } });
        assert!(parse_tool_events(DelegateAgent::Gemini, &line, 0).is_empty());
        // A claude line with no tool blocks (plain assistant text) lifts nothing.
        let text_only = serde_json::json!({ "type": "assistant",
            "message": { "content": [{ "type": "text", "text": "hi" }] } });
        assert!(parse_tool_events(DelegateAgent::Claude, &text_only, 0).is_empty());
        // A codex envelope (agent_message) lifts nothing.
        let envelope = serde_json::json!({ "type": "item", "item": { "type": "agent_message", "text": "done" } });
        assert!(parse_tool_events(DelegateAgent::Codex, &envelope, 0).is_empty());
    }

    #[test]
    fn long_title_is_truncated_on_a_char_boundary() {
        let long = "x".repeat(300);
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "name": "Bash", "input": { "command": long } },
            ] }
        });
        let events = parse_tool_events(DelegateAgent::Claude, &line, 0);
        match &events[0] {
            EventKind::ToolStarted { title: Some(t), .. } => {
                assert_eq!(t.len(), MAX_TOOL_TITLE, "title bounded to the cap");
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    /// Push a tape the way the capture path does (jobs.rs): each raw line pushes an
    /// `Output` then its lifted tool events, in tape order. Seals it through a real
    /// store and returns the persisted events' kind tags.
    fn capture_kind_tags(agent: DelegateAgent, stream: &[&str]) -> Vec<String> {
        let mut writer = crate::run_store::RunWriter::begin("job-1", agent.catalog_name(), None);
        writer.push(EventKind::RunStarted {
            agent: agent.catalog_name().into(),
            task: "t".into(),
            cwd: None,
            inputs: None,
        });
        writer.push(EventKind::TurnStarted { turn: 0 });
        for line in stream {
            writer.push(EventKind::Output {
                turn: 0,
                text: (*line).to_string(),
            });
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                for kind in parse_tool_events(agent, &value, 0) {
                    writer.push(kind);
                }
            }
        }
        writer.push(EventKind::TurnFinished { turn: 0, ok: true });
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::run_store::RunStore::new(dir.path().to_path_buf());
        let sealed = writer.seal(true, Some(&store)).expect("sealed run");
        let run = store.load_record(&sealed.run_id).expect("load sealed run");
        run.events
            .iter()
            .map(|e| {
                serde_json::to_value(&e.kind).expect("event kind json")["kind"]
                    .as_str()
                    .expect("kind tag")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn captured_claude_stream_seals_tool_events_in_tape_order() {
        // A tool_use line then its matching tool_result line: the sealed tape
        // interleaves Output -> ToolStarted, then Output -> ToolFinished, in order.
        let stream = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":"a\nb","is_error":false}]}}"#,
        ];
        let tags = capture_kind_tags(DelegateAgent::Claude, &stream);
        assert_eq!(
            tags,
            vec![
                "run_started",
                "turn_started",
                "output",
                "tool_started",
                "output",
                "tool_finished",
                "turn_finished",
            ],
            "tool events sit in tape order right after their Output line"
        );
    }

    #[test]
    fn captured_gemini_and_plain_stream_yields_no_tool_events() {
        // Gemini lifts nothing -> its tape is Output-only (UNCHANGED from pre-Wave-2):
        // no tool_started/tool_finished anywhere.
        let stream = [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"X","input":{}}]}}"#,
            r#"{"type":"result","result":"done"}"#,
        ];
        let tags = capture_kind_tags(DelegateAgent::Gemini, &stream);
        assert_eq!(
            tags,
            vec![
                "run_started",
                "turn_started",
                "output",
                "output",
                "turn_finished",
            ],
            "a gemini/plain tape carries no tool events"
        );
        assert!(!tags.iter().any(|t| t.starts_with("tool_")));
    }
}
