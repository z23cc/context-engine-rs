use crate::{RiskTier, RuntimeCommand, RuntimeJobError, Strategy};
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
    /// Streaming output fragment from a delegated external agent CLI, scoped to
    /// its job. `agent` is the catalog name (codex / claude / gemini); `text` is a
    /// raw stdout/stderr chunk. Additive and job-scoped; the producer is wired in
    /// DA-2 (this variant only reserves the protocol shape).
    DelegateProgress {
        job_id: String,
        agent: String,
        text: String,
    },
    /// A flow run (the Conductor, design §4) has started. Carries the declarative
    /// [`Strategy`] so a client can render the DAG shape before any node runs. All
    /// `flow_*` events carry the `flow_id` (which is the flow job's id).
    FlowStarted {
        flow_id: String,
        strategy: Strategy,
    },
    /// A flow node's worker has started. `worker` is a human-readable label (the
    /// CLI catalog name or `provider/model`); `kind` is the worker family
    /// (`cli` | `provider`) so a client can badge the node pane.
    FlowNodeStarted {
        flow_id: String,
        node_id: String,
        worker: String,
        kind: FlowWorkerKind,
    },
    /// A flow node's worker has finished. `ok` is the node's success; `usage` is
    /// the node's token usage (zeroed when the worker reported none).
    FlowNodeFinished {
        flow_id: String,
        node_id: String,
        ok: bool,
        usage: FlowNodeUsage,
    },
    /// A DAG edge `from → to` between two flow nodes, for rendering the graph.
    /// Emitted as the engine wires a downstream node to its upstream producer.
    FlowEdge {
        flow_id: String,
        from: String,
        to: String,
    },
    /// A structured agent-loop step scoped to a flow node — **reuses
    /// [`AgentEventKind`] verbatim**, symmetric with [`Self::SessionAgent`], so the
    /// TUI renders a node pane exactly as a session pane keyed by `node_id`.
    FlowNodeAgent {
        flow_id: String,
        node_id: String,
        event: AgentEventKind,
    },
    /// A flow finished with an aggregated outcome (the fold of the recorded node
    /// results, in declared order — design §3).
    FlowCompleted {
        flow_id: String,
        outcome: FlowRunOutcome,
    },
    /// A flow failed. `node_id` names the offending node when the failure is
    /// node-local; `error` is a human-readable message.
    FlowFailed {
        flow_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        node_id: Option<String>,
        error: String,
    },
}

/// Which worker family ran a flow node — the only place the CLI-vs-provider
/// distinction is visible to a flow client (design §2/§7). Protocol data; the
/// host maps its own worker kind onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FlowWorkerKind {
    /// An external agentic CLI (codex / claude / gemini) subprocess.
    Cli,
    /// An in-process provider loop over the Nerve tool surface.
    Provider,
}

/// A flow node's token usage, carried on [`RuntimeEvent::FlowNodeFinished`].
/// Mirrors the token fields of [`AgentEventKind::Usage`]; cache counts are
/// optional and omitted when the worker did not report caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
pub struct FlowNodeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u64>,
}

/// The aggregated outcome of a finished flow, carried on
/// [`RuntimeEvent::FlowCompleted`]: whether the flow succeeded under its
/// join/fail policy, a one-line summary, and the flow's final text (the kept
/// results concatenated, in declared order).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
pub struct FlowRunOutcome {
    pub ok: bool,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_text: String,
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
            | Self::FlowFailed { flow_id, .. } => Some(flow_id.as_str()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Step, TaskTemplate, WorkerRef};

    fn single_strategy() -> Strategy {
        Strategy::Single {
            step: Step {
                worker: WorkerRef::Cli {
                    name: "claude".into(),
                },
                task: TaskTemplate::new("do it"),
                autonomy: crate::DelegateAutonomy::ReadOnly,
                on_fail: crate::FailPolicy::Abort,
            },
        }
    }

    #[test]
    fn flow_started_serializes_strategy_and_is_flow_scoped() {
        let event = RuntimeEvent::flow_started("flow-1", single_strategy());
        let value = serde_json::to_value(&event).expect("flow_started json");
        assert_eq!(value["type"], "flow_started");
        assert_eq!(value["flow_id"], "flow-1");
        assert_eq!(value["strategy"]["type"], "single");
        // Flow events route through the per-id fan-out via session_id() -> flow_id.
        assert_eq!(event.session_id(), Some("flow-1"));
        // Round-trips back to an equal event.
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, event);
    }

    #[test]
    fn flow_node_events_round_trip() {
        let started =
            RuntimeEvent::flow_node_started("flow-1", "branch-0", "claude", FlowWorkerKind::Cli);
        let value = serde_json::to_value(&started).expect("node_started json");
        assert_eq!(value["type"], "flow_node_started");
        assert_eq!(value["node_id"], "branch-0");
        assert_eq!(value["worker"], "claude");
        assert_eq!(value["kind"], "cli");
        assert_eq!(started.session_id(), Some("flow-1"));

        let finished = RuntimeEvent::flow_node_finished(
            "flow-1",
            "branch-0",
            true,
            FlowNodeUsage {
                input_tokens: 5,
                output_tokens: 3,
                ..FlowNodeUsage::default()
            },
        );
        let value = serde_json::to_value(&finished).expect("node_finished json");
        assert_eq!(value["type"], "flow_node_finished");
        assert_eq!(value["ok"], true);
        assert_eq!(value["usage"]["input_tokens"], 5);
        // Zero cache counts are omitted (optional, skip_serializing_if).
        assert!(value["usage"].get("cache_read_tokens").is_none());
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, finished);
    }

    #[test]
    fn flow_node_agent_reuses_agent_event_kind() {
        // Symmetric with SessionAgent: a flow node pane renders the same shape.
        let event = RuntimeEvent::flow_node_agent(
            "flow-1",
            "node-0",
            AgentEventKind::Message {
                text: "hello".into(),
            },
        );
        let value = serde_json::to_value(&event).expect("node_agent json");
        assert_eq!(value["type"], "flow_node_agent");
        assert_eq!(value["event"]["kind"], "message");
        assert_eq!(value["event"]["text"], "hello");
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, event);
    }

    #[test]
    fn flow_edge_and_completed_and_failed_round_trip() {
        let edge = RuntimeEvent::flow_edge("flow-1", "node-0", "node-1");
        let value = serde_json::to_value(&edge).expect("edge json");
        assert_eq!(value["type"], "flow_edge");
        assert_eq!(value["from"], "node-0");
        assert_eq!(value["to"], "node-1");

        let completed = RuntimeEvent::flow_completed(
            "flow-1",
            FlowRunOutcome {
                ok: true,
                summary: "single: ok".into(),
                final_text: "the answer".into(),
            },
        );
        let value = serde_json::to_value(&completed).expect("completed json");
        assert_eq!(value["type"], "flow_completed");
        assert_eq!(value["outcome"]["ok"], true);
        assert_eq!(value["outcome"]["final_text"], "the answer");
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, completed);

        let failed = RuntimeEvent::flow_failed("flow-1", Some("branch-2".into()), "worker died");
        let value = serde_json::to_value(&failed).expect("failed json");
        assert_eq!(value["type"], "flow_failed");
        assert_eq!(value["node_id"], "branch-2");
        assert_eq!(value["error"], "worker died");
        // A node-less failure omits node_id.
        let global = RuntimeEvent::flow_failed("flow-1", None, "budget exhausted");
        let value = serde_json::to_value(&global).expect("global failed json");
        assert!(value.get("node_id").is_none());
    }
}
