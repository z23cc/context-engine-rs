//! Shared mapping from agent-loop events to the protocol's transport-neutral
//! [`AgentEventKind`]. Used by both the job adapter (`agent.run`) and the
//! session manager (`session.*`), which differ only in the outer event wrapper
//! (`RuntimeEvent::agent` vs `RuntimeEvent::session_agent`).

use nerve_agent::AgentEvent;
use nerve_runtime::AgentEventKind;

/// Translate an [`AgentEvent`] into a protocol [`AgentEventKind`], or `None` for
/// events that carry no client-visible step (the terminal `Done`).
pub(crate) fn agent_event_kind(event: AgentEvent) -> Option<AgentEventKind> {
    Some(match event {
        AgentEvent::TurnStarted(turn) => AgentEventKind::TurnStarted {
            turn: u64::from(turn),
        },
        AgentEvent::AssistantText(text) => AgentEventKind::Message { text },
        AgentEvent::Reasoning(text) => AgentEventKind::Reasoning { text },
        AgentEvent::ToolStarted { name, args } => AgentEventKind::ToolStarted {
            tool: name,
            arguments: args,
        },
        AgentEvent::ToolFinished { name, ok, output } => AgentEventKind::ToolFinished {
            tool: name,
            ok,
            output,
        },
        AgentEvent::Interrupted(reason) => AgentEventKind::Interrupted { reason },
        AgentEvent::Usage {
            input_tokens,
            output_tokens,
        } => AgentEventKind::Usage {
            input_tokens: u64::from(input_tokens),
            output_tokens: u64::from(output_tokens),
        },
        AgentEvent::Done { .. } => return None,
    })
}
