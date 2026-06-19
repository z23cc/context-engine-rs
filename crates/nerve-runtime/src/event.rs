use crate::{RiskTier, RuntimeCommand, RuntimeJobError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default risk tier for an [`RuntimeEvent::ApprovalRequested`] whose `tier` field
/// is absent on the wire: the most-restricted tier, so an omitted classification
/// is never treated as safer than it is.
fn default_approval_tier() -> RiskTier {
    RiskTier::Exec
}

/// Runtime event emitted by human-facing adapters while executing jobs.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    JobStarted {
        job_id: String,
        command: String,
        tool_name: Option<String>,
    },
    JobProgress {
        job_id: String,
        stage: String,
        message: String,
        current: Option<u64>,
        total: Option<u64>,
    },
    JobCancelRequested {
        job_id: String,
    },
    JobCompleted {
        job_id: String,
    },
    JobFailed {
        job_id: String,
        error: RuntimeJobError,
    },
    JobCancelled {
        job_id: String,
    },
    /// A structured step from the built-in agent loop, scoped to its job.
    Agent {
        job_id: String,
        event: AgentEventKind,
    },
    /// A host-managed session has been created or resumed.
    SessionStarted {
        session_id: String,
    },
    /// A host-managed session has started processing a user turn.
    TurnStarted {
        session_id: String,
    },
    /// A host-managed session is ready for the next client action.
    SessionIdle {
        session_id: String,
    },
    /// A host-managed session has been closed.
    SessionClosed {
        session_id: String,
    },
    /// A structured agent-loop step scoped to an interactive session.
    SessionAgent {
        session_id: String,
        event: AgentEventKind,
    },
    /// Advisory streaming fragment of an in-progress tool call, scoped to its
    /// job. Carries a raw provider delta string; UI-only and additive — clients
    /// that don't render streaming tool calls may ignore it. The producer is
    /// wired in a later wave; this variant only reserves the protocol shape.
    ToolCallDelta {
        job_id: String,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<u64>,
    },
    /// A session turn needs a client/human decision before continuing.
    ApprovalRequested {
        session_id: String,
        request_id: String,
        tool: String,
        arguments: Value,
        /// Risk classification of the tool whose call is awaiting a decision.
        /// Additive; older emitters/clients that omit it default to the
        /// most-restricted tier ([`RiskTier::Exec`]).
        #[serde(default = "default_approval_tier")]
        tier: RiskTier,
        /// Human-readable preview of what the call would do (e.g. a diff or
        /// command line). Additive; defaults to empty when not computed.
        #[serde(default)]
        preview: String,
    },
    /// A host-managed authentication lifecycle update.
    Auth {
        provider: String,
        kind: AuthEventKind,
    },
}

/// Authentication lifecycle event kind. Defined as pure protocol data; hosts map
/// concrete credential/login implementation details onto these states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuthEventKind {
    LoginPending,
    LoginCompleted,
    LoginFailed,
    CredentialRefreshed,
}

/// Payload of a [`RuntimeEvent::Agent`] — one step of the agent loop. Defined as
/// transport-neutral data; the host maps its own agent events onto these.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEventKind {
    TurnStarted {
        turn: u64,
    },
    Message {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolStarted {
        tool: String,
        arguments: Value,
    },
    ToolFinished {
        tool: String,
        ok: bool,
        output: String,
    },
    Interrupted {
        reason: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        /// Prompt tokens served from the provider's prompt cache, when reported.
        /// Additive and optional: producers that don't track caching omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_read_tokens: Option<u64>,
        /// Prompt tokens written into the provider's prompt cache, when reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_creation_tokens: Option<u64>,
    },
}

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
            Self::JobStarted { .. }
            | Self::JobProgress { .. }
            | Self::JobCancelRequested { .. }
            | Self::JobCompleted { .. }
            | Self::JobFailed { .. }
            | Self::JobCancelled { .. }
            | Self::Agent { .. }
            | Self::ToolCallDelta { .. }
            | Self::Auth { .. } => None,
        }
    }
}
