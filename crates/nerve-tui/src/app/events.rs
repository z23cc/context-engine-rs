//! Mapping runtime events → shell state, and the key actions the loop performs.
//!
//! Kept separate from the IO loop so the event→state reduction is unit-testable
//! without a terminal or a live daemon. Mirrors the relevant arms of the TS
//! `#onEvent` / `#onAgentEvent`.

use nerve_runtime::{AgentEventKind, RuntimeEvent};

use super::state::{ApprovalState, Mode, State, Tone};

/// Apply one runtime event to the shell state. Returns `true` if the frame
/// should be re-rendered. Only the subset the minimal shell understands is
/// handled; everything else is ignored (additive-safe).
pub fn apply_event(state: &mut State, event: &RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::SessionStarted { session_id } => {
            state.session_id = Some(session_id.clone());
            true
        }
        RuntimeEvent::TurnStarted { .. } => {
            state.running = true;
            state.turn_started_at = Some(std::time::Instant::now());
            state.elapsed_ms = 0;
            true
        }
        RuntimeEvent::SessionIdle { .. } => {
            state.running = false;
            state.end_stream();
            true
        }
        RuntimeEvent::ApprovalRequested {
            session_id,
            request_id,
            tool,
            arguments,
            tier,
            preview,
        } => {
            // Stage the modal state; render/handling live in `render`/`input`.
            // Mirrors the TS `approval_requested` arm (compact-JSON args).
            state.mode = Mode::Approval;
            state.approval = Some(ApprovalState {
                tool: tool.clone(),
                args: args_to_string(arguments),
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                tier: *tier,
                preview: preview.clone(),
            });
            true
        }
        RuntimeEvent::SessionAgent { event, .. } => apply_agent_event(state, event),
        RuntimeEvent::JobFailed { error, .. } => {
            // A second message racing an in-flight turn: the genuine turn is still
            // live, so hint rather than clearing `running` / dumping a red line.
            if error.message.contains("is already running") {
                state.hint = "still working — Ctrl-C to interrupt".to_string();
            } else {
                state.running = false;
                state.push_notice(Tone::Error, error.message.clone());
            }
            true
        }
        _ => false,
    }
}

fn apply_agent_event(state: &mut State, event: &AgentEventKind) -> bool {
    match event {
        // Empty deltas are dropped (providers emit trailing empty chunks); the
        // append helpers no-op on "", but skipping here also avoids a redraw.
        AgentEventKind::Message { text } => {
            if text.is_empty() {
                return false;
            }
            state.append_assistant(text);
            true
        }
        AgentEventKind::Reasoning { text } => {
            if text.is_empty() {
                return false;
            }
            state.append_reasoning(text);
            true
        }
        AgentEventKind::ToolStarted { tool, arguments } => {
            state.start_tool(tool.clone(), args_to_string(arguments));
            true
        }
        AgentEventKind::ToolFinished { tool, ok, output } => {
            state.finish_tool(tool, *ok, output.clone());
            true
        }
        AgentEventKind::Interrupted { reason } => {
            state.push_notice(Tone::Warn, format!("interrupted: {reason}"));
            true
        }
        // Usage feeds the status bar (tokens / context % / cost).
        AgentEventKind::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            state.record_usage(*input_tokens, *output_tokens);
            true
        }
        // TurnStarted is handled at the RuntimeEvent layer.
        AgentEventKind::TurnStarted { .. } => false,
    }
}

/// Serialize tool arguments to a compact JSON string for the cell header. A JSON
/// string value is unquoted; everything else is its JSON encoding (mirrors the TS
/// `safeJson`).
fn args_to_string(arguments: &serde_json::Value) -> String {
    match arguments {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Block;
    use nerve_runtime::RuntimeJobError;

    #[test]
    fn session_started_records_id() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(&mut state, &RuntimeEvent::session_started("sess-1"));
        assert!(redraw);
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn turn_started_and_idle_toggle_running() {
        let mut state = State::new("p", "m");
        apply_event(&mut state, &RuntimeEvent::turn_started("s"));
        assert!(state.running);
        apply_event(&mut state, &RuntimeEvent::session_idle("s"));
        assert!(!state.running);
    }

    #[test]
    fn agent_message_streams_into_assistant_block() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Message { text: "ab".into() }),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Message { text: "cd".into() }),
        );
        assert_eq!(state.blocks, vec![Block::Assistant("abcd".to_string())]);
    }

    #[test]
    fn job_failed_clears_running_and_notes_error() {
        let mut state = State::new("p", "m");
        state.running = true;
        apply_event(
            &mut state,
            &RuntimeEvent::job_failed("j", RuntimeJobError::new("k", "boom")),
        );
        assert!(!state.running);
        assert!(matches!(
            state.blocks.last(),
            Some(Block::Notice { tone: Tone::Error, text }) if text.contains("boom")
        ));
    }

    #[test]
    fn agent_reasoning_streams_into_reasoning_block() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Reasoning { text: "th".into() }),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Reasoning { text: "ink".into() }),
        );
        assert_eq!(state.blocks, vec![Block::Reasoning("think".to_string())]);
    }

    #[test]
    fn tool_started_then_finished_builds_a_tool_block() {
        use crate::app::state::ToolStatus;
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent(
                "s",
                AgentEventKind::ToolStarted {
                    tool: "read_file".into(),
                    arguments: serde_json::json!({ "path": "a.rs" }),
                },
            ),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent(
                "s",
                AgentEventKind::ToolFinished {
                    tool: "read_file".into(),
                    ok: true,
                    output: "contents".into(),
                },
            ),
        );
        let Some(Block::Tool(cell)) = state.blocks.last() else {
            panic!("expected a tool block");
        };
        assert_eq!(cell.status, ToolStatus::Ok);
        assert_eq!(cell.tool, "read_file");
        assert_eq!(cell.output.as_deref(), Some("contents"));
    }

    #[test]
    fn empty_agent_delta_does_not_push_or_redraw() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::session_agent(
                "s",
                AgentEventKind::Message {
                    text: String::new(),
                },
            ),
        );
        assert!(!redraw);
        assert!(state.blocks.is_empty());
    }

    #[test]
    fn unknown_event_does_not_redraw() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(&mut state, &RuntimeEvent::job_completed("j"));
        assert!(!redraw);
    }
}
