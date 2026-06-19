//! Mapping runtime events → shell state, and the key actions the loop performs.
//!
//! Kept separate from the IO loop so the event→state reduction is unit-testable
//! without a terminal or a live daemon. Mirrors the relevant arms of the TS
//! `#onEvent` / `#onAgentEvent`.

use nerve_runtime::{AgentEventKind, RuntimeEvent};

use super::state::State;

/// Apply one runtime event to the shell state. Returns `true` if the frame
/// should be re-rendered. Only the subset the minimal shell understands is
/// handled; everything else is ignored (additive-safe).
pub fn apply_event(state: &mut State, event: &RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::SessionStarted { session_id } => {
            state.session_id = Some(session_id.clone());
            state.note("session ready");
            true
        }
        RuntimeEvent::TurnStarted { .. } => {
            state.running = true;
            true
        }
        RuntimeEvent::SessionIdle { .. } => {
            state.running = false;
            state.end_stream();
            true
        }
        RuntimeEvent::SessionAgent { event, .. } => apply_agent_event(state, event),
        RuntimeEvent::JobFailed { error, .. } => {
            state.running = false;
            state.note(format!("error: {}", error.message));
            true
        }
        _ => false,
    }
}

fn apply_agent_event(state: &mut State, event: &AgentEventKind) -> bool {
    match event {
        AgentEventKind::Message { text } => {
            state.append_assistant(text);
            true
        }
        AgentEventKind::ToolStarted { tool, .. } => {
            state.end_stream();
            state.note(format!("· {tool}"));
            true
        }
        AgentEventKind::Interrupted { reason } => {
            state.note(format!("interrupted: {reason}"));
            true
        }
        // Reasoning/tool-finished/usage are rendered richly in T2; the minimal
        // shell ignores them.
        _ => false,
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
        assert!(matches!(state.blocks.last(), Some(Block::Notice(t)) if t.contains("boom")));
    }

    #[test]
    fn unknown_event_does_not_redraw() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(&mut state, &RuntimeEvent::job_completed("j"));
        assert!(!redraw);
    }
}
