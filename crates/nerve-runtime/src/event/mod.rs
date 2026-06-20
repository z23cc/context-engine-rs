mod ctor;

use crate::{RiskTier, RuntimeJobError, Strategy};
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
    /// Running fleet-budget telemetry (design §6): the cumulative cost + tokens
    /// spent across the whole flow tree, emitted after each node's `Usage` is
    /// debited from the deterministic `BudgetLedger`. Additive within v4.
    BudgetUpdate {
        flow_id: String,
        spent_usd: f64,
        tokens: u64,
    },
    /// A fleet-budget warning (design §6): spend crossed a threshold (e.g. 80%)
    /// of a configured limit but has not yet been exhausted. `limit_usd` is the
    /// USD ceiling the warning is relative to. Additive within v4.
    BudgetWarning {
        flow_id: String,
        spent_usd: f64,
        limit_usd: f64,
    },
    /// A typed, replayable audit-trail decision the engine made (design §4/§6):
    /// a budget exhaustion, a depth/worker spawn refusal (absence-at-floor), and
    /// — in later waves — a vote tally / judge pick / debate round. `node_id`
    /// names the node the decision is about (the synthetic root `"flow"` for a
    /// flow-wide decision); `kind` is the [`FlowDecisionKind`]. Additive within v4.
    FlowDecision {
        flow_id: String,
        node_id: String,
        kind: FlowDecisionKind,
    },
}

/// The typed kinds a [`RuntimeEvent::FlowDecision`] can record (design §6/§8).
/// A closed, additive-versioned enum so the audit trail is golden-diffable and
/// replayable; later waves add vote/judge/debate kinds.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowDecisionKind {
    /// The fleet budget (USD or tokens) was exhausted; the engine cooperatively
    /// cancelled every branch (design §6, the budget brake / fork-bomb cure).
    BudgetExhausted,
    /// A spawn was refused at the depth ceiling — the engine did not start more
    /// workers because `depth >= max_depth` (design §8, absence-at-floor). A
    /// deterministic, recorded refusal, not a crash.
    DepthCeiling { depth: u32, max_depth: u32 },
    /// A spawn was refused at the worker ceiling — `live_workers >= max_workers`
    /// across the whole tree (design §8, the process-global semaphore bound).
    WorkerCeiling { live_workers: u32, max_workers: u32 },
    /// A `VoteJudge` strategy tallied its candidates before adjudication (design §3):
    /// how many candidate workers succeeded out of how many ran, and whether the
    /// quorum `k` was reached. The audit trail of WHAT the judge was handed. Additive
    /// within v4.
    VoteTally {
        ok: u32,
        total: u32,
        k: u32,
        reached: bool,
    },
    /// A judge (a `VoteJudge` or `Debate` strategy's adjudicator) picked an outcome
    /// (design §3): the judge node's id and whether it succeeded. The audit trail of
    /// the adjudication itself. Additive within v4.
    JudgePick { node_id: String, ok: bool },
    /// One round of a `Debate` strategy completed (design §3): the round index
    /// (0-based) and how many of the sides argued successfully that round. Emitted
    /// once per round so the debate's progression is a replayable audit trail.
    /// Additive within v4.
    DebateRound { round: u32, sides_ok: u32 },
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

    #[test]
    fn budget_update_and_warning_round_trip_and_are_flow_scoped() {
        let update = RuntimeEvent::budget_update("flow-1", 0.42, 1234);
        let value = serde_json::to_value(&update).expect("budget_update json");
        assert_eq!(value["type"], "budget_update");
        assert_eq!(value["spent_usd"], 0.42);
        assert_eq!(value["tokens"], 1234);
        // Budget events route through the per-id fan-out (so a flow client sees them).
        assert_eq!(update.session_id(), Some("flow-1"));
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, update);

        let warning = RuntimeEvent::budget_warning("flow-1", 0.8, 1.0);
        let value = serde_json::to_value(&warning).expect("budget_warning json");
        assert_eq!(value["type"], "budget_warning");
        assert_eq!(value["limit_usd"], 1.0);
        assert_eq!(warning.session_id(), Some("flow-1"));
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, warning);
    }

    #[test]
    fn flow_decision_round_trips_each_kind() {
        let exhausted =
            RuntimeEvent::flow_decision("flow-1", "flow", FlowDecisionKind::BudgetExhausted);
        let value = serde_json::to_value(&exhausted).expect("decision json");
        assert_eq!(value["type"], "flow_decision");
        assert_eq!(value["node_id"], "flow");
        assert_eq!(value["kind"]["kind"], "budget_exhausted");
        assert_eq!(exhausted.session_id(), Some("flow-1"));
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, exhausted);

        let depth = RuntimeEvent::flow_decision(
            "flow-1",
            "branch-3",
            FlowDecisionKind::DepthCeiling {
                depth: 2,
                max_depth: 2,
            },
        );
        let value = serde_json::to_value(&depth).expect("depth json");
        assert_eq!(value["kind"]["kind"], "depth_ceiling");
        assert_eq!(value["kind"]["depth"], 2);
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, depth);

        let workers = RuntimeEvent::flow_decision(
            "flow-1",
            "branch-4",
            FlowDecisionKind::WorkerCeiling {
                live_workers: 4,
                max_workers: 4,
            },
        );
        let value = serde_json::to_value(&workers).expect("workers json");
        assert_eq!(value["kind"]["kind"], "worker_ceiling");
        assert_eq!(value["kind"]["max_workers"], 4);
        let back: RuntimeEvent = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, workers);
    }

    #[test]
    fn flow_decision_round_trips_c5_audit_kinds() {
        // The richer-strategy audit kinds (C5): a vote tally, a judge pick, a debate
        // round — all additive within v4.
        let tally = RuntimeEvent::flow_decision(
            "flow-1",
            "flow",
            FlowDecisionKind::VoteTally {
                ok: 2,
                total: 3,
                k: 2,
                reached: true,
            },
        );
        let value = serde_json::to_value(&tally).expect("tally json");
        assert_eq!(value["kind"]["kind"], "vote_tally");
        assert_eq!(value["kind"]["ok"], 2);
        assert_eq!(value["kind"]["reached"], true);
        assert_eq!(
            serde_json::from_value::<RuntimeEvent>(value).expect("round-trip"),
            tally
        );

        let pick = RuntimeEvent::flow_decision(
            "flow-1",
            "judge",
            FlowDecisionKind::JudgePick {
                node_id: "judge".into(),
                ok: true,
            },
        );
        let value = serde_json::to_value(&pick).expect("pick json");
        assert_eq!(value["kind"]["kind"], "judge_pick");
        assert_eq!(value["kind"]["node_id"], "judge");
        assert_eq!(
            serde_json::from_value::<RuntimeEvent>(value).expect("round-trip"),
            pick
        );

        let round = RuntimeEvent::flow_decision(
            "flow-1",
            "flow",
            FlowDecisionKind::DebateRound {
                round: 1,
                sides_ok: 2,
            },
        );
        let value = serde_json::to_value(&round).expect("round json");
        assert_eq!(value["kind"]["kind"], "debate_round");
        assert_eq!(value["kind"]["round"], 1);
        assert_eq!(value["kind"]["sides_ok"], 2);
        assert_eq!(
            serde_json::from_value::<RuntimeEvent>(value).expect("round-trip"),
            round
        );
    }
}
