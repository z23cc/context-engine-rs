//! The `impl RuntimeEvent` block (split out of `event/mod.rs` for the file-size
//! convention): the ergonomic constructors the host uses to emit events, plus the
//! `session_id()` per-id routing accessor. The event TYPES live in the sibling
//! [`super`] module. Pure data construction — no logic.

use super::{
    AgentEventKind, AuthEventKind, FlowDecisionKind, FlowNodeUsage, FlowRunOutcome, FlowWorkerKind,
};
use crate::{RiskTier, RuntimeCommand, RuntimeEvent, RuntimeJobError, Strategy};
use serde_json::Value;

impl RuntimeEvent {
    #[must_use]
    pub fn auth(provider: impl Into<String>, kind: AuthEventKind) -> Self {
        Self::Auth {
            provider: provider.into(),
            kind,
        }
    }

    #[must_use]
    pub fn agent(job_id: impl Into<String>, event: AgentEventKind) -> Self {
        Self::Agent {
            job_id: job_id.into(),
            event,
        }
    }

    #[must_use]
    pub fn session_agent(session_id: impl Into<String>, event: AgentEventKind) -> Self {
        Self::SessionAgent {
            session_id: session_id.into(),
            event,
        }
    }

    #[must_use]
    pub fn tool_call_delta(
        job_id: impl Into<String>,
        delta: impl Into<String>,
        index: Option<u64>,
    ) -> Self {
        Self::ToolCallDelta {
            job_id: job_id.into(),
            delta: delta.into(),
            index,
        }
    }

    #[must_use]
    pub fn delegate_progress(
        job_id: impl Into<String>,
        agent: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self::DelegateProgress {
            job_id: job_id.into(),
            agent: agent.into(),
            text: text.into(),
        }
    }

    #[must_use]
    pub fn flow_started(flow_id: impl Into<String>, strategy: Strategy) -> Self {
        Self::FlowStarted {
            flow_id: flow_id.into(),
            strategy,
        }
    }

    #[must_use]
    pub fn flow_node_started(
        flow_id: impl Into<String>,
        node_id: impl Into<String>,
        worker: impl Into<String>,
        kind: FlowWorkerKind,
    ) -> Self {
        Self::FlowNodeStarted {
            flow_id: flow_id.into(),
            node_id: node_id.into(),
            worker: worker.into(),
            kind,
        }
    }

    #[must_use]
    pub fn flow_node_finished(
        flow_id: impl Into<String>,
        node_id: impl Into<String>,
        ok: bool,
        usage: FlowNodeUsage,
    ) -> Self {
        Self::FlowNodeFinished {
            flow_id: flow_id.into(),
            node_id: node_id.into(),
            ok,
            usage,
        }
    }

    #[must_use]
    pub fn flow_edge(
        flow_id: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self::FlowEdge {
            flow_id: flow_id.into(),
            from: from.into(),
            to: to.into(),
        }
    }

    #[must_use]
    pub fn flow_node_agent(
        flow_id: impl Into<String>,
        node_id: impl Into<String>,
        event: AgentEventKind,
    ) -> Self {
        Self::FlowNodeAgent {
            flow_id: flow_id.into(),
            node_id: node_id.into(),
            event,
        }
    }

    #[must_use]
    pub fn flow_completed(flow_id: impl Into<String>, outcome: FlowRunOutcome) -> Self {
        Self::FlowCompleted {
            flow_id: flow_id.into(),
            outcome,
        }
    }

    #[must_use]
    pub fn flow_failed(
        flow_id: impl Into<String>,
        node_id: Option<String>,
        error: impl Into<String>,
    ) -> Self {
        Self::FlowFailed {
            flow_id: flow_id.into(),
            node_id,
            error: error.into(),
        }
    }

    #[must_use]
    pub fn budget_update(flow_id: impl Into<String>, spent_usd: f64, tokens: u64) -> Self {
        Self::BudgetUpdate {
            flow_id: flow_id.into(),
            spent_usd,
            tokens,
        }
    }

    #[must_use]
    pub fn budget_warning(flow_id: impl Into<String>, spent_usd: f64, limit_usd: f64) -> Self {
        Self::BudgetWarning {
            flow_id: flow_id.into(),
            spent_usd,
            limit_usd,
        }
    }

    #[must_use]
    pub fn flow_decision(
        flow_id: impl Into<String>,
        node_id: impl Into<String>,
        kind: FlowDecisionKind,
    ) -> Self {
        Self::FlowDecision {
            flow_id: flow_id.into(),
            node_id: node_id.into(),
            kind,
        }
    }

    #[must_use]
    pub fn session_started(session_id: impl Into<String>) -> Self {
        Self::SessionStarted {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn turn_started(session_id: impl Into<String>) -> Self {
        Self::TurnStarted {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn session_idle(session_id: impl Into<String>) -> Self {
        Self::SessionIdle {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn session_closed(session_id: impl Into<String>) -> Self {
        Self::SessionClosed {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn approval_requested(
        session_id: impl Into<String>,
        request_id: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
        tier: RiskTier,
        preview: impl Into<String>,
    ) -> Self {
        Self::ApprovalRequested {
            session_id: session_id.into(),
            request_id: request_id.into(),
            tool: tool.into(),
            arguments,
            tier,
            preview: preview.into(),
        }
    }

    #[must_use]
    pub fn job_started(job_id: impl Into<String>, command: &RuntimeCommand) -> Self {
        Self::JobStarted {
            job_id: job_id.into(),
            command: command.name().to_string(),
            tool_name: command.tool_name().map(str::to_string),
        }
    }

    #[must_use]
    pub fn job_progress(
        job_id: impl Into<String>,
        stage: impl Into<String>,
        message: impl Into<String>,
        current: Option<u64>,
        total: Option<u64>,
    ) -> Self {
        Self::JobProgress {
            job_id: job_id.into(),
            stage: stage.into(),
            message: message.into(),
            current,
            total,
        }
    }

    #[must_use]
    pub fn job_cancel_requested(job_id: impl Into<String>) -> Self {
        Self::JobCancelRequested {
            job_id: job_id.into(),
        }
    }

    #[must_use]
    pub fn job_completed(job_id: impl Into<String>) -> Self {
        Self::JobCompleted {
            job_id: job_id.into(),
        }
    }

    #[must_use]
    pub fn job_failed(job_id: impl Into<String>, error: RuntimeJobError) -> Self {
        Self::JobFailed {
            job_id: job_id.into(),
            error,
        }
    }

    #[must_use]
    pub fn job_cancelled(job_id: impl Into<String>) -> Self {
        Self::JobCancelled {
            job_id: job_id.into(),
        }
    }

    /// The session this event belongs to, if it is session-scoped. Job- and
    /// auth-scoped events return `None`. Transports use this to fan a frame out
    /// only to subscribers watching that session (a session subscriber sees its
    /// own session-scoped events plus all unscoped ones). Accessor only — the
    /// wire shape (the `type`-tagged enum) is unchanged.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        match self {
            Self::SessionStarted { session_id }
            | Self::TurnStarted { session_id }
            | Self::SessionIdle { session_id }
            | Self::SessionClosed { session_id }
            | Self::SessionAgent { session_id, .. }
            | Self::ApprovalRequested { session_id, .. } => Some(session_id.as_str()),
            // A flow is just another id stream: returning `flow_id` here routes the
            // per-id event fan-out and the existing TUI approval modal to a flow
            // with zero client change (design §4). The `flow_id` IS the flow job id.
            Self::FlowStarted { flow_id, .. }
            | Self::FlowNodeStarted { flow_id, .. }
            | Self::FlowNodeFinished { flow_id, .. }
            | Self::FlowEdge { flow_id, .. }
            | Self::FlowNodeAgent { flow_id, .. }
            | Self::FlowCompleted { flow_id, .. }
            | Self::FlowFailed { flow_id, .. }
            | Self::BudgetUpdate { flow_id, .. }
            | Self::BudgetWarning { flow_id, .. }
            | Self::FlowDecision { flow_id, .. } => Some(flow_id.as_str()),
            Self::JobStarted { .. }
            | Self::JobProgress { .. }
            | Self::JobCancelRequested { .. }
            | Self::JobCompleted { .. }
            | Self::JobFailed { .. }
            | Self::JobCancelled { .. }
            | Self::Agent { .. }
            | Self::ToolCallDelta { .. }
            | Self::DelegateProgress { .. }
            | Self::Auth { .. } => None,
        }
    }
}
