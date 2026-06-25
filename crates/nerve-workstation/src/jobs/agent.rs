//! The `agent.run` command executor for the [`JobManager`].
//!
//! Building the orchestrator is a composition-root concern (hence here rather than in
//! `nerve-runtime`); this module streams the run's agent events as `runtime/event`
//! notifications and reports the run outcome as the job result.

use super::JobManager;
use crate::agent;
use crate::policy::ToolGate;
use nerve_agent::AgentEvent;
use nerve_core::CancelToken;
use nerve_runtime::{RuntimeCommand, RuntimeEvent};
use serde_json::{Value, json};
use std::sync::Arc;

impl JobManager {
    /// Execute an `agent.run` command: build the orchestrator (composition root
    /// concern, hence here rather than in `nerve-runtime`) and stream its agent
    /// events as `runtime/event` notifications. The job result is the run outcome.
    pub(super) fn run_agent_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let RuntimeCommand::AgentRun {
            provider,
            model,
            task,
            system_prompt,
            max_turns,
            temperature,
            reasoning_effort,
            tool_filter,
        } = command
        else {
            return Err(nerve_runtime::RuntimeError::adapter(
                "expected agent.run command",
            ));
        };
        let config = agent::AgentRunConfig {
            workspace: None,
            provider,
            model,
            task,
            system_prompt,
            max_turns,
            temperature,
            reasoning_effort,
            tool_filter,
            api_key: None,
            distill_memory: false,
            verify_completion: false,
            // Daemon-served runs refuse exec by trust context, not just by flag.
            allow_exec: false,
            exec_launcher: crate::sandbox::refuse_launcher(),
            // One-shot daemon `agent.run` jobs don't expose the delegate tool;
            // delegation is the dedicated `delegate.start` job (DA-2) and the
            // session chat-tool path (DA-3). Refuse by trust context here.
            allow_delegate: false,
            delegate_launcher: crate::sandbox::refuse_launcher(),
            delegate_event_sink: None,
            // One-shot agent.run jobs start fresh (resume is the session layer).
            resume_truncations: 0,
            // Cost budget guard is opt-in; off for daemon agent.run jobs.
            cost_budget_usd: None,
        };
        let emit = Arc::clone(&self.emit);
        let job_id = job_id.to_string();
        let mut sink = move |event: AgentEvent| {
            if let Some(runtime_event) = map_agent_event(&job_id, event) {
                emit(runtime_event);
            }
        };
        // Daemon is non-interactive: deny on `Ask` (safe default). A real
        // approval round-trip over the protocol is future Session-layer work.
        let gate = ToolGate::deny(self.policy.clone());
        match agent::run_agent(
            Arc::clone(&self.runtime),
            config,
            &self.registry,
            gate,
            token,
            &mut sink,
            self.session_store.as_ref(),
        ) {
            Ok(outcome) => Ok(json!({
                "reason": outcome.reason,
                "turns": outcome.turns,
                "final_text": outcome.final_text,
                "usage": {
                    "input_tokens": outcome.usage.input_tokens,
                    "output_tokens": outcome.usage.output_tokens,
                },
            })),
            Err(_) if token.is_cancelled() => Err(nerve_runtime::RuntimeError::cancelled()),
            Err(err) => Err(nerve_runtime::RuntimeError::adapter(err.to_string())),
        }
    }
}

fn map_agent_event(job_id: &str, event: AgentEvent) -> Option<RuntimeEvent> {
    // Streaming tool-call fragments map to the job-scoped `ToolCallDelta`
    // RuntimeEvent (advisory/UI-only) rather than a structured agent step.
    if let AgentEvent::ToolCallDelta { name, arguments } = &event {
        let delta = tool_call_delta_payload(name, arguments);
        return Some(RuntimeEvent::tool_call_delta(
            job_id.to_string(),
            delta,
            None,
        ));
    }
    crate::agent_event::agent_event_kind(event)
        .map(|kind| RuntimeEvent::agent(job_id.to_string(), kind))
}

/// Render an advisory tool-call delta as a compact `name(arguments)` string for
/// the UI-only `ToolCallDelta` event. The wire shape carries a raw delta string.
fn tool_call_delta_payload(name: &str, arguments: &serde_json::Value) -> String {
    format!("{name}({arguments})")
}
