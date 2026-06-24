//! SSE event routing: fold each `RuntimeEvent` from `/events` into the chat
//! that owns its session. Split out of `app.rs` to stay under the file-size
//! gate; the chat state types live in [`crate::app`].

use crate::app::{ApprovalReq, Chat, Role, ToolCard, Turn};
use leptos::prelude::*;
use nerve_proto::{AgentEventKind, RuntimeEvent};

/// Route one `RuntimeEvent` from the SSE stream into the chat owning its session.
pub(crate) fn route_event(
    event: RuntimeEvent,
    chats: RwSignal<Vec<Chat>>,
    approval: RwSignal<Option<ApprovalReq>>,
) {
    match event {
        RuntimeEvent::SessionIdle { session_id } => with_session(chats, &session_id, end_turn),
        RuntimeEvent::SessionClosed { session_id } => with_session(chats, &session_id, end_turn),
        // A turn's job reaching a terminal state ends the turn — keyed by the
        // turn's job id (start job for turn 1, or the steer job). Without these,
        // only SessionIdle clears `streaming`, so any server-side failure (CLI
        // crash, missing/unauthenticated CLI, cancelled steer) wedges the chat
        // spinning forever with the composer locked to Stop.
        RuntimeEvent::JobFailed { job_id, error } => with_turn_job(chats, &job_id, |c| {
            end_turn(c);
            append_assistant_text(c, &format!("\n\n⚠ {}", error.message));
        }),
        RuntimeEvent::JobCancelled { job_id } => with_turn_job(chats, &job_id, end_turn),
        RuntimeEvent::JobCompleted { job_id } => with_turn_job(chats, &job_id, end_turn),
        RuntimeEvent::SessionAgent { session_id, event } => {
            with_session(chats, &session_id, |c| apply_agent_event(event, c));
        }
        // Job-scoped agent steps (the own-engine `session.*`/`agent.run` path emits
        // `Agent` keyed by job id rather than `SessionAgent` by session id). Without
        // this arm an own-engine turn streams nothing into the transcript.
        RuntimeEvent::Agent { job_id, event } => {
            with_turn_job(chats, &job_id, |c| apply_agent_event(event, c));
        }
        // The delegate path (local CLIs) streams raw assistant text chunks keyed by
        // the delegate session/job id — coalesce them into the streaming turn.
        RuntimeEvent::DelegateProgress { job_id, text, .. } => {
            with_session(chats, &job_id, |c| append_assistant_text(c, &text));
        }
        RuntimeEvent::ApprovalRequested {
            session_id,
            request_id,
            tool,
            preview,
            tier,
            ..
        }
            // Only surface approvals for a session we own.
            if chats.with_untracked(|cs| {
                cs.iter()
                    .any(|c| c.session.as_deref() == Some(session_id.as_str()))
            }) => {
                approval.set(Some(ApprovalReq {
                    session_id,
                    request_id,
                    tool,
                    preview,
                    tier: format!("{tier:?}"),
                }));
            }
        _ => {}
    }
}

/// Append streamed assistant `text` to the chat's current streaming turn, opening
/// one if the last turn is not a live assistant turn. Shared by the delegate
/// stream (`DelegateProgress`) and the structured `Message` agent event.
pub(crate) fn append_assistant_text(chat: &mut Chat, text: &str) {
    let needs_turn =
        !matches!(chat.turns.last(), Some(t) if t.role == Role::Assistant && t.streaming);
    if needs_turn {
        chat.turns.push(Turn::assistant_streaming());
    }
    if let Some(turn) = chat.turns.last_mut() {
        turn.text.push_str(text);
    }
}

/// Apply `f` to the chat whose session matches `session_id`, if any.
fn with_session(chats: RwSignal<Vec<Chat>>, session_id: &str, f: impl FnOnce(&mut Chat)) {
    chats.update(|cs| {
        if let Some(c) = cs
            .iter_mut()
            .find(|c| c.session.as_deref() == Some(session_id))
        {
            f(c);
        }
    });
}

/// Apply `f` to the chat whose in-flight turn job matches `job_id`, if any.
fn with_turn_job(chats: RwSignal<Vec<Chat>>, job_id: &str, f: impl FnOnce(&mut Chat)) {
    chats.update(|cs| {
        if let Some(c) = cs
            .iter_mut()
            .find(|c| c.turn_job.as_deref() == Some(job_id))
        {
            f(c);
        }
    });
}

/// Mark a chat's in-flight turn finished: stop streaming + drop the turn job.
fn end_turn(chat: &mut Chat) {
    chat.streaming = false;
    chat.turn_job = None;
    if let Some(turn) = chat.turns.last_mut() {
        turn.streaming = false;
    }
}

/// Fold a single `AgentEventKind` into the chat's current (streaming) assistant turn.
fn apply_agent_event(event: AgentEventKind, chat: &mut Chat) {
    let needs_turn =
        !matches!(chat.turns.last(), Some(t) if t.role == Role::Assistant && t.streaming);
    if needs_turn {
        chat.turns.push(Turn::assistant_streaming());
    }
    let Some(turn) = chat.turns.last_mut() else {
        return;
    };
    match event {
        AgentEventKind::Message { text } => turn.text.push_str(&text),
        AgentEventKind::Reasoning { text } => turn.reasoning.push_str(&text),
        AgentEventKind::ToolStarted { tool, arguments } => turn.tools.push(ToolCard {
            tool,
            ok: None,
            input: format_arguments(&arguments),
            output: String::new(),
        }),
        AgentEventKind::ToolFinished { tool, ok, output } => {
            match turn
                .tools
                .iter_mut()
                .rev()
                .find(|card| card.tool == tool && card.ok.is_none())
            {
                Some(card) => {
                    card.ok = Some(ok);
                    card.output = output;
                }
                None => turn.tools.push(ToolCard {
                    tool,
                    ok: Some(ok),
                    input: String::new(),
                    output,
                }),
            }
        }
        AgentEventKind::Interrupted { .. } => {
            turn.streaming = false;
            chat.streaming = false;
        }
        AgentEventKind::TurnStarted { .. } | AgentEventKind::Usage { .. } => {}
    }
}

fn format_arguments(arguments: &serde_json::Value) -> String {
    if arguments.is_null() {
        return String::new();
    }
    serde_json::to_string_pretty(arguments).unwrap_or_else(|_| arguments.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Chat;

    /// A host-constructible empty chat (struct literal avoids the wasm-only
    /// `js_sys::Date::now()` in `Chat::new_with_backend`).
    fn empty_chat() -> Chat {
        Chat {
            title: "t".into(),
            backend: "claude".into(),
            agent: "claude".into(),
            session: None,
            turn_job: None,
            turns: Vec::new(),
            streaming: false,
            updated_ms: 0.0,
        }
    }

    #[test]
    fn append_assistant_text_opens_one_streaming_turn_then_coalesces() {
        let mut c = empty_chat();
        append_assistant_text(&mut c, "hello ");
        append_assistant_text(&mut c, "world");
        assert_eq!(
            c.turns.len(),
            1,
            "chunks coalesce into a single streaming turn"
        );
        assert!(matches!(c.turns[0].role, Role::Assistant));
        assert!(c.turns[0].streaming);
        assert_eq!(c.turns[0].text, "hello world");
    }

    #[test]
    fn append_after_a_finished_turn_opens_a_fresh_turn() {
        let mut c = empty_chat();
        append_assistant_text(&mut c, "first");
        end_turn(&mut c);
        append_assistant_text(&mut c, "second");
        assert_eq!(c.turns.len(), 2);
        assert_eq!(c.turns[1].text, "second");
    }

    #[test]
    fn end_turn_clears_streaming_and_turn_job() {
        let mut c = empty_chat();
        c.streaming = true;
        c.turn_job = Some("job-1".into());
        append_assistant_text(&mut c, "answer");
        end_turn(&mut c);
        assert!(!c.streaming);
        assert!(c.turn_job.is_none());
        assert!(!c.turns.last().unwrap().streaming);
    }

    #[test]
    fn message_and_reasoning_route_to_distinct_fields() {
        let mut c = empty_chat();
        apply_agent_event(
            AgentEventKind::Reasoning {
                text: "thinking".into(),
            },
            &mut c,
        );
        apply_agent_event(
            AgentEventKind::Message {
                text: "answer".into(),
            },
            &mut c,
        );
        let turn = c.turns.last().expect("turn");
        assert_eq!(turn.reasoning, "thinking");
        assert_eq!(turn.text, "answer");
    }

    #[test]
    fn tool_started_then_finished_updates_the_same_card() {
        let mut c = empty_chat();
        apply_agent_event(
            AgentEventKind::ToolStarted {
                tool: "read_file".into(),
                arguments: serde_json::json!({ "path": "src/lib.rs" }),
            },
            &mut c,
        );
        apply_agent_event(
            AgentEventKind::ToolFinished {
                tool: "read_file".into(),
                ok: true,
                output: "ok".into(),
            },
            &mut c,
        );
        let tools = &c.turns.last().expect("turn").tools;
        assert_eq!(
            tools.len(),
            1,
            "finish updates the started card, not a new one"
        );
        assert_eq!(tools[0].ok, Some(true));
        assert_eq!(tools[0].output, "ok");
        assert!(tools[0].input.contains("path"));
    }

    #[test]
    fn tool_finished_without_a_start_pushes_a_card() {
        let mut c = empty_chat();
        apply_agent_event(
            AgentEventKind::ToolFinished {
                tool: "edit".into(),
                ok: false,
                output: "boom".into(),
            },
            &mut c,
        );
        let tools = &c.turns.last().expect("turn").tools;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].ok, Some(false));
    }
}
