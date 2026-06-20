//! Per-turn run-config assembly and agent→protocol event mapping for a session.
//!
//! Split out of [`super`] so the manager module stays focused on session
//! lifecycle (start / turn / resume / approval). These are pure functions over a
//! [`SessionConfig`] and the resolved agent def.

use super::SessionConfig;
use crate::agent;
use crate::capabilities::ResolvedAgent;
use crate::delegate_tool::DelegateProgressSink;
use crate::sandbox::SandboxLauncher;
use nerve_agent::AgentEvent;
use nerve_runtime::RuntimeEvent;
use std::sync::Arc;

/// Build the [`AgentRunConfig`](agent::AgentRunConfig) for one session turn,
/// layering the session config over the resolved agent def. A session turn always
/// refuses exec by trust context; delegation is the daemon's `--allow-delegate`
/// lift threaded through `allow_delegate` + `delegate_launcher`, with progress
/// streamed via `delegate_event_sink`. The delegate tool is exposed only at depth
/// 0 (see `assemble_toolbox`), so sub-agents never delegate.
pub(super) fn session_run_config(
    config: &SessionConfig,
    resolved: ResolvedAgent,
    task: &str,
    allow_delegate: bool,
    delegate_launcher: Arc<dyn SandboxLauncher>,
    delegate_event_sink: Option<Arc<DelegateProgressSink>>,
) -> agent::AgentRunConfig {
    agent::AgentRunConfig {
        workspace: config.workspace.clone(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        task: task.to_string(),
        system_prompt: config.system_prompt.clone().or(resolved.system_prompt),
        max_turns: config.max_turns.or(resolved.max_turns),
        temperature: config.temperature.or(resolved.temperature),
        reasoning_effort: config
            .reasoning_effort
            .clone()
            .or(resolved.reasoning_effort),
        tool_filter: config.tool_filter.clone().or(resolved.tool_filter),
        api_key: None,
        distill_memory: false,
        verify_completion: false,
        // Daemon session turns refuse exec by trust context, not just by flag.
        allow_exec: false,
        exec_launcher: crate::sandbox::refuse_launcher(),
        // Delegation is the daemon's `--allow-delegate` lift (same flag as the
        // delegate.start job launcher), threaded through here.
        allow_delegate,
        delegate_launcher,
        delegate_event_sink,
        // Carry the resumed truncation counter into the orchestrator's ResumeState.
        resume_truncations: config.resume_truncations,
        // Session turns don't impose a cost budget guard (opt-in elsewhere).
        cost_budget_usd: None,
    }
}

/// Map a streamed [`AgentEvent`] to its session-scoped [`RuntimeEvent`], dropping
/// events that have no protocol projection.
pub(super) fn map_session_agent_event(session_id: &str, event: AgentEvent) -> Option<RuntimeEvent> {
    crate::agent_event::agent_event_kind(event)
        .map(|kind| RuntimeEvent::session_agent(session_id.to_string(), kind))
}
