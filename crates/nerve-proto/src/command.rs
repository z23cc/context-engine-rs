use crate::{RiskTier, WorkflowDef};
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Runtime command kinds accepted by the human-facing daemon job protocol.
pub const RUNTIME_COMMAND_NAMES: &[&str] = &[
    "ping",
    "tool.list",
    "tool.call",
    "agent.run",
    "session.start",
    "session.message",
    "session.interrupt",
    "session.respond",
    "session.get",
    "session.list",
    "session.close",
    "session.set_model",
    "session.set_mode",
    "auth.start",
    "auth.complete",
    "auth.status",
    "auth.logout",
    "delegate.start",
    "delegate.steer",
    "delegate.close",
    "flow.start",
    "flow.steer",
    "flow.replay",
    "flow.get",
    "flow.list",
    "flow.close",
    "flow.respond",
];

/// Transport-neutral command understood by human-facing runtime adapters.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "kind")]
pub enum RuntimeCommand {
    /// Lightweight health check used by clients before opening a real session.
    #[serde(rename = "ping")]
    Ping,
    /// Return all runtime tool specifications.
    #[serde(rename = "tool.list")]
    ToolList,
    /// Execute one MCP-style tool through the runtime dispatcher.
    #[serde(rename = "tool.call")]
    ToolCall {
        name: String,
        #[serde(default = "default_arguments")]
        arguments: BTreeMap<String, Value>,
    },
    /// Run the built-in agent loop as a job. This is protocol vocabulary only:
    /// the host job manager (the composition root) executes it; the core runtime
    /// dispatcher does not (it has no LLM/provider knowledge). Provider/model are
    /// plain data here and translated to domain types by the host.
    #[serde(rename = "agent.run")]
    AgentRun {
        provider: String,
        model: String,
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_turns: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_filter: Option<Vec<String>>,
    },
    /// Start or resume a host-managed interactive agent session.
    #[serde(rename = "session.start")]
    SessionStart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        provider: String,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_turns: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        temperature: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_filter: Option<Vec<String>>,
    },
    /// Send a user message to an existing host-managed session.
    #[serde(rename = "session.message")]
    SessionMessage { session_id: String, text: String },
    /// Interrupt the current turn of an existing host-managed session.
    #[serde(rename = "session.interrupt")]
    SessionInterrupt { session_id: String },
    /// Reply to a session approval request.
    #[serde(rename = "session.respond")]
    SessionRespond {
        session_id: String,
        request_id: String,
        decision: SessionApprovalDecision,
    },
    /// Fetch one host-managed session.
    #[serde(rename = "session.get")]
    SessionGet { session_id: String },
    /// List host-managed sessions.
    #[serde(rename = "session.list")]
    SessionList,
    /// Close a host-managed session.
    #[serde(rename = "session.close")]
    SessionClose { session_id: String },
    /// Switch the model (and optionally provider) of a live session in place,
    /// keeping its history and checkpoint. Takes effect from the next turn.
    #[serde(rename = "session.set_model")]
    SessionSetModel {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        model: String,
    },
    /// Switch the approval mode of a live session in place. Takes effect from the
    /// next gate decision. Pure protocol vocabulary; the host session manager
    /// stores it (P2 consults it in the gate).
    #[serde(rename = "session.set_mode")]
    SessionSetMode {
        session_id: String,
        mode: ApprovalMode,
    },
    /// Start a host-managed OAuth login and return an authorization URL.
    #[serde(rename = "auth.start")]
    AuthStart { provider: String },
    /// Complete a host-managed OAuth login with a code or pasted callback URL.
    #[serde(rename = "auth.complete")]
    AuthComplete {
        login_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        callback_url: Option<String>,
    },
    /// Return stored OAuth/API-key credential status without secrets.
    #[serde(rename = "auth.status")]
    AuthStatus { provider: String },
    /// Remove stored credentials for a provider.
    #[serde(rename = "auth.logout")]
    AuthLogout { provider: String },
    /// Delegate a coding task to an external agent CLI (codex / claude / gemini)
    /// as a long-lived job. Pure protocol vocabulary: the host job manager drives
    /// the subprocess (DA-2); `nerve-core` has no subprocess knowledge. `agent` is
    /// the catalog name from `list_agents`; `cwd` defaults to the workspace root;
    /// `model` overrides the agent's default model. Progress streams back as
    /// [`crate::RuntimeEvent::DelegateProgress`].
    #[serde(rename = "delegate.start")]
    DelegateStart {
        agent: String,
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default)]
        autonomy: DelegateAutonomy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// DA-6 (codex only): the MCP allowlist for this delegated codex session —
        /// the `[mcp_servers.<name>]` entries to keep enabled; every other
        /// configured server is disabled for a fast start. `Some(list)` overrides
        /// the persisted `[delegate.codex] mcp_enable` config (an empty list
        /// disables ALL); `None` falls back to that config. Ignored for non-codex
        /// agents.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mcp_enable: Option<Vec<String>>,
    },
    /// Steer a live delegated session with a follow-up user message, running one
    /// more turn against the same long-lived agent process. Pure protocol
    /// vocabulary: the host job manager looks up the live session (DA-5a) and
    /// continues it; progress streams back as [`crate::RuntimeEvent::DelegateProgress`].
    /// `session_id` is the `job_id` of the originating [`Self::DelegateStart`] job
    /// (a started delegated session keeps that id for its whole lifetime).
    #[serde(rename = "delegate.steer")]
    DelegateSteer { session_id: String, message: String },
    /// End a live delegated session: close the agent process's stdin (which it
    /// treats as EOF and exits on) and reap it. Pure protocol vocabulary; the host
    /// job manager deregisters the live session. `session_id` is the originating
    /// [`Self::DelegateStart`] job id (see [`Self::DelegateSteer`]).
    #[serde(rename = "delegate.close")]
    DelegateClose { session_id: String },
    /// Start a declarative orchestration workflow (the Conductor, design §4) as one
    /// cancellable **job**: the host job manager runs the deterministic flow engine
    /// (C1) and the `job_id` IS the `flow_id`. The `workflow` is either an inline
    /// [`WorkflowDef`] or a named reference resolved from a loaded `WorkflowDef`
    /// data file ([`FlowSource`]). Progress streams back as the `flow_*` events
    /// ([`crate::RuntimeEvent::FlowStarted`] …); approvals reuse
    /// [`crate::RuntimeEvent::ApprovalRequested`] keyed by `flow_id`.
    #[serde(rename = "flow.start")]
    FlowStart {
        #[serde(flatten)]
        workflow: FlowSource,
        /// Named-output seeds the engine exposes to the first node's task template.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inputs: Option<BTreeMap<String, String>>,
        /// Workspace to run against when more than one is registered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Steer a live flow branch with a follow-up message, running one more turn
    /// against the same live worker session (design §4 / Wave C3a). Reuses the C0
    /// [`WorkerSession::steer`] port per-worker, exactly as `delegate.steer` does
    /// for a single delegated session: the host job manager looks up the live
    /// worker for `target` in the live-flow worker registry and continues it;
    /// progress streams back as [`crate::RuntimeEvent::FlowNodeAgent`]. Only a
    /// live, steerable branch (a `Single`/`Pipeline` worker still in flight, on a
    /// steerable substrate) can be steered; a closed or one-shot worker
    /// (`gemini`) returns a clear error. `flow_id` is the originating
    /// [`Self::FlowStart`] job id; `target` selects which branch (by node id, or
    /// the only live worker when unset).
    #[serde(rename = "flow.steer")]
    FlowSteer {
        flow_id: String,
        #[serde(default, skip_serializing_if = "WorkerSelector::is_default")]
        target: WorkerSelector,
        message: String,
    },
    /// Deterministically REPLAY a recorded flow offline (the audit verb, design §3/§4).
    /// The host loads the recorded [`WorkerLedger`] from the `FlowStore` by
    /// [`LedgerRef`], runs the SAME deterministic engine in REPLAY mode — a
    /// `ReplayWorker` re-emits the recorded `WorkerEvent`s/`TurnResult`s instead of
    /// calling any LLM/subprocess — and re-emits the `flow_*` event stream. Runs as
    /// one cancellable **job** (the `job_id` IS the replayed `flow_id`); the replay is
    /// byte-identical to the recorded run (the CI gate), at zero cost. `workspace`
    /// scopes which project's `.nerve/flows` the ledger is loaded from when more than
    /// one is registered.
    #[serde(rename = "flow.replay")]
    FlowReplay {
        #[serde(flatten)]
        ledger_ref: LedgerRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    /// Fetch one live or recently-finished flow by id. Mirrors `session.get`.
    #[serde(rename = "flow.get")]
    FlowGet { flow_id: String },
    /// List flows the host knows about. Mirrors `session.list`.
    #[serde(rename = "flow.list")]
    FlowList,
    /// Close (cancel) a live flow, tearing its workers down. Mirrors
    /// `session.close` / `delegate.close`. `flow_id` is the originating
    /// [`Self::FlowStart`] job id.
    #[serde(rename = "flow.close")]
    FlowClose { flow_id: String },
    /// Reply to a flow approval request. **Reuses** the existing
    /// [`SessionApprovalDecision`] + the host `ApprovalHub` round-trip, keyed by
    /// `flow_id` (a flow branch is just another approval id). No new approval type.
    #[serde(rename = "flow.respond")]
    FlowRespond {
        flow_id: String,
        request_id: String,
        decision: SessionApprovalDecision,
    },
}

/// The workflow a [`RuntimeCommand::FlowStart`] runs: either an **inline**
/// [`WorkflowDef`] or a **named reference** to a loaded `WorkflowDef` data file
/// (design §4 / §6, the P3 workflow-defs surface). Untagged so a client can send
/// either `{ "workflow": { ... } }` or `{ "workflow_ref": "name" }` inside the
/// `flow.start` command without an extra discriminant.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(untagged)]
pub enum FlowSource {
    /// An inline workflow definition (the whole strategy as data). Boxed so the
    /// large inline-def variant doesn't bloat every command value (the named-ref
    /// variant is tiny).
    Inline { workflow: Box<WorkflowDef> },
    /// A named workflow resolved from a loaded data file (loaded, not compiled).
    Named { workflow_ref: String },
}

/// Which recorded ledger a [`RuntimeCommand::FlowReplay`] replays (design §4/§5).
/// Untagged so a client sends either `{ "flow_id": "job-7" }` (resolve the ledger
/// from the `FlowStore` by flow id — the common case) or `{ "ledger_path": "…" }`
/// (an explicit `.nerve/flows/<id>/ledger.jsonl`-shaped path), without an extra
/// discriminant. Deliberately MINIMAL — a closed two-arm enum, not a query language.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(untagged)]
pub enum LedgerRef {
    /// Resolve the recorded ledger from the `FlowStore` by its `flow_id`.
    FlowId { flow_id: String },
    /// Replay a ledger at an explicit filesystem path (e.g. a copied tape).
    Path { ledger_path: String },
}

/// Which live branch a [`RuntimeCommand::FlowSteer`] targets (design §4, the
/// `WorkerSelector` row). Deliberately MINIMAL: a flow branch is identified by its
/// deterministic node id, or — when `node_id` is unset — "the single live worker"
/// (the common case for a `Single`/`Pipeline` flow, which has exactly one live
/// branch at a time). An ambiguous unset selector against a multi-branch live flow
/// is refused by the host with a clear message; the closed enum keeps the steer
/// surface from drifting into a query language.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct WorkerSelector {
    /// The deterministic node id of the branch to steer (e.g. `"node-0"` for a
    /// `Single` flow, `"stage-1"` for a pipeline's second stage). `None` targets
    /// the only live worker, erroring if more than one is live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl WorkerSelector {
    /// A selector targeting a specific node id.
    #[must_use]
    pub fn node(node_id: impl Into<String>) -> Self {
        Self {
            node_id: Some(node_id.into()),
        }
    }

    /// Whether this is the default ("only live worker") selector — used to keep an
    /// unset `target` off the wire (serde `skip_serializing_if`).
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.node_id.is_none()
    }
}

/// Autonomy posture handed to a delegated external agent CLI, mapping to each
/// vendor's sandbox/permission flag: codex `--sandbox`, claude `--permission-mode`,
/// gemini `--approval-mode` (read-only | edit | full). Defaults to the most
/// restricted ([`Self::ReadOnly`]) so an omitted field never grants more than read
/// access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DelegateAutonomy {
    /// The delegated agent may only read; no edits, no command execution.
    #[default]
    ReadOnly,
    /// The delegated agent may read and edit workspace files.
    Edit,
    /// The delegated agent may read, edit, and run commands.
    Full,
}

/// Decision supplied by a human/client for a session approval request.
///
/// `Allow`/`Deny` apply to this call only; `AllowAlways`/`DenyAlways` additionally
/// signal the host to remember the decision for future calls (P2 wires the
/// remembering; P1 only distinguishes allow-vs-deny via [`Self::allows`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SessionApprovalDecision {
    /// Allow this call only.
    Allow,
    /// Deny this call only.
    Deny,
    /// Allow this call and remember the allow for future matching calls.
    AllowAlways,
    /// Deny this call and remember the deny for future matching calls.
    DenyAlways,
}

impl SessionApprovalDecision {
    /// Whether the decision permits the call (either the one-shot or remembered
    /// allow). Consumers should compare with this rather than `== Allow` so the
    /// remembered variant is not silently treated as a deny.
    #[must_use]
    pub fn allows(&self) -> bool {
        matches!(self, Self::Allow | Self::AllowAlways)
    }

    /// Whether the host should persist this decision for future matching calls.
    #[must_use]
    pub fn remember(&self) -> bool {
        matches!(self, Self::AllowAlways | Self::DenyAlways)
    }
}

/// Per-session approval posture controlling how high a [`RiskTier`] the gate may
/// auto-approve without prompting. Pure protocol data; the host gate (P2) maps
/// each tool's tier against [`Self::max_auto_tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Prompt for everything above read-only.
    AlwaysAsk,
    /// Auto-approve reads and edits; prompt for exec.
    Write,
    /// Auto-approve everything, including exec.
    Yolo,
}

impl ApprovalMode {
    /// Highest tier this mode auto-approves without prompting: anything at or
    /// below it is allowed, anything above it requires an approval round-trip.
    #[must_use]
    pub fn max_auto_tier(self) -> RiskTier {
        match self {
            Self::AlwaysAsk => RiskTier::ReadOnly,
            Self::Write => RiskTier::Edit,
            Self::Yolo => RiskTier::Exec,
        }
    }
}

impl RuntimeCommand {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::ToolList => "tool.list",
            Self::ToolCall { .. } => "tool.call",
            Self::AgentRun { .. } => "agent.run",
            Self::SessionStart { .. } => "session.start",
            Self::SessionMessage { .. } => "session.message",
            Self::SessionInterrupt { .. } => "session.interrupt",
            Self::SessionRespond { .. } => "session.respond",
            Self::SessionGet { .. } => "session.get",
            Self::SessionList => "session.list",
            Self::SessionClose { .. } => "session.close",
            Self::SessionSetModel { .. } => "session.set_model",
            Self::SessionSetMode { .. } => "session.set_mode",
            Self::AuthStart { .. } => "auth.start",
            Self::AuthComplete { .. } => "auth.complete",
            Self::AuthStatus { .. } => "auth.status",
            Self::AuthLogout { .. } => "auth.logout",
            Self::DelegateStart { .. } => "delegate.start",
            Self::DelegateSteer { .. } => "delegate.steer",
            Self::DelegateClose { .. } => "delegate.close",
            Self::FlowStart { .. } => "flow.start",
            Self::FlowSteer { .. } => "flow.steer",
            Self::FlowReplay { .. } => "flow.replay",
            Self::FlowGet { .. } => "flow.get",
            Self::FlowList => "flow.list",
            Self::FlowClose { .. } => "flow.close",
            Self::FlowRespond { .. } => "flow.respond",
        }
    }

    #[must_use]
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::ToolCall { name, .. } => Some(name.as_str()),
            Self::Ping
            | Self::ToolList
            | Self::AgentRun { .. }
            | Self::SessionStart { .. }
            | Self::SessionMessage { .. }
            | Self::SessionInterrupt { .. }
            | Self::SessionRespond { .. }
            | Self::SessionGet { .. }
            | Self::SessionList
            | Self::SessionClose { .. }
            | Self::SessionSetModel { .. }
            | Self::SessionSetMode { .. }
            | Self::AuthStart { .. }
            | Self::AuthComplete { .. }
            | Self::AuthStatus { .. }
            | Self::AuthLogout { .. }
            | Self::DelegateStart { .. }
            | Self::DelegateSteer { .. }
            | Self::DelegateClose { .. }
            | Self::FlowStart { .. }
            | Self::FlowSteer { .. }
            | Self::FlowReplay { .. }
            | Self::FlowGet { .. }
            | Self::FlowList
            | Self::FlowClose { .. }
            | Self::FlowRespond { .. } => None,
        }
    }
}

fn default_arguments() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_set_model_round_trips() {
        let value = serde_json::json!({
            "kind": "session.set_model",
            "session_id": "s1",
            "model": "grok-4-fast",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse set_model");
        assert_eq!(command.name(), "session.set_model");
        assert_eq!(command.tool_name(), None);
        match command {
            RuntimeCommand::SessionSetModel {
                session_id,
                provider,
                model,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(provider, None);
                assert_eq!(model, "grok-4-fast");
            }
            other => panic!("unexpected variant: {}", other.name()),
        }
        // session.set_model is listed in the canonical command-name set.
        assert!(RUNTIME_COMMAND_NAMES.contains(&"session.set_model"));
    }

    #[test]
    fn session_set_mode_round_trips() {
        let value = serde_json::json!({
            "kind": "session.set_mode",
            "session_id": "s1",
            "mode": "write",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse set_mode");
        assert_eq!(command.name(), "session.set_mode");
        assert_eq!(command.tool_name(), None);
        match command {
            RuntimeCommand::SessionSetMode { session_id, mode } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, ApprovalMode::Write);
            }
            other => panic!("unexpected variant: {}", other.name()),
        }
        assert!(RUNTIME_COMMAND_NAMES.contains(&"session.set_mode"));
    }

    #[test]
    fn delegate_start_round_trips_with_default_autonomy() {
        // `autonomy` and `model`/`cwd` omitted: autonomy defaults to the most
        // restricted tier, optionals to None.
        let value = serde_json::json!({
            "kind": "delegate.start",
            "agent": "codex",
            "task": "add a test",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse delegate.start");
        assert_eq!(command.name(), "delegate.start");
        assert_eq!(command.tool_name(), None);
        match command {
            RuntimeCommand::DelegateStart {
                agent,
                task,
                cwd,
                autonomy,
                model,
                mcp_enable,
            } => {
                assert_eq!(agent, "codex");
                assert_eq!(task, "add a test");
                assert_eq!(cwd, None);
                assert_eq!(autonomy, DelegateAutonomy::ReadOnly);
                assert_eq!(model, None);
                assert_eq!(mcp_enable, None);
            }
            other => panic!("unexpected variant: {}", other.name()),
        }
        assert!(RUNTIME_COMMAND_NAMES.contains(&"delegate.start"));
    }

    #[test]
    fn delegate_start_round_trips_mcp_enable_allowlist() {
        // DA-6: a per-call codex MCP allowlist round-trips (and an empty list is a
        // valid override meaning "disable all").
        let value = serde_json::json!({
            "kind": "delegate.start",
            "agent": "codex",
            "task": "investigate",
            "mcp_enable": ["chrome-devtools"],
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse with allowlist");
        match command {
            RuntimeCommand::DelegateStart { mcp_enable, .. } => {
                assert_eq!(mcp_enable, Some(vec!["chrome-devtools".to_string()]));
            }
            other => panic!("unexpected variant: {}", other.name()),
        }

        // Re-serialize: `mcp_enable` is present when Some, absent when None.
        let with = RuntimeCommand::DelegateStart {
            agent: "codex".into(),
            task: "t".into(),
            cwd: None,
            autonomy: DelegateAutonomy::ReadOnly,
            model: None,
            mcp_enable: Some(vec![]),
        };
        let json = serde_json::to_value(&with).expect("serialize Some([])");
        assert_eq!(json["mcp_enable"], serde_json::json!([]));
        let without = RuntimeCommand::DelegateStart {
            agent: "codex".into(),
            task: "t".into(),
            cwd: None,
            autonomy: DelegateAutonomy::ReadOnly,
            model: None,
            mcp_enable: None,
        };
        let json = serde_json::to_value(&without).expect("serialize None");
        assert!(json.get("mcp_enable").is_none(), "None is skipped: {json}");
    }

    #[test]
    fn delegate_steer_and_close_round_trip() {
        let steer: RuntimeCommand = serde_json::from_value(serde_json::json!({
            "kind": "delegate.steer",
            "session_id": "job-7",
            "message": "now run the tests",
        }))
        .expect("parse delegate.steer");
        assert_eq!(steer.name(), "delegate.steer");
        assert_eq!(steer.tool_name(), None);
        match steer {
            RuntimeCommand::DelegateSteer {
                session_id,
                message,
            } => {
                assert_eq!(session_id, "job-7");
                assert_eq!(message, "now run the tests");
            }
            other => panic!("unexpected variant: {}", other.name()),
        }

        let close: RuntimeCommand = serde_json::from_value(serde_json::json!({
            "kind": "delegate.close",
            "session_id": "job-7",
        }))
        .expect("parse delegate.close");
        assert_eq!(close.name(), "delegate.close");
        assert_eq!(close.tool_name(), None);
        match close {
            RuntimeCommand::DelegateClose { session_id } => assert_eq!(session_id, "job-7"),
            other => panic!("unexpected variant: {}", other.name()),
        }

        assert!(RUNTIME_COMMAND_NAMES.contains(&"delegate.steer"));
        assert!(RUNTIME_COMMAND_NAMES.contains(&"delegate.close"));
    }

    #[test]
    fn delegate_autonomy_serde_names_and_default() {
        assert_eq!(DelegateAutonomy::default(), DelegateAutonomy::ReadOnly);
        for (autonomy, name) in [
            (DelegateAutonomy::ReadOnly, "read_only"),
            (DelegateAutonomy::Edit, "edit"),
            (DelegateAutonomy::Full, "full"),
        ] {
            assert_eq!(
                serde_json::to_value(autonomy).unwrap(),
                serde_json::json!(name)
            );
        }
    }

    #[test]
    fn approval_mode_serde_names_and_tiers() {
        for (mode, name, tier) in [
            (ApprovalMode::AlwaysAsk, "always_ask", RiskTier::ReadOnly),
            (ApprovalMode::Write, "write", RiskTier::Edit),
            (ApprovalMode::Yolo, "yolo", RiskTier::Exec),
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), serde_json::json!(name));
            assert_eq!(mode.max_auto_tier(), tier);
        }
    }

    #[test]
    fn flow_start_round_trips_inline_workflow() {
        // An inline WorkflowDef parses through the untagged `FlowSource`.
        let value = serde_json::json!({
            "kind": "flow.start",
            "workflow": {
                "schema_version": 1,
                "name": "fan",
                "strategy": {
                    "type": "single",
                    "step": {
                        "worker": { "kind": "cli", "name": "claude" },
                        "task": "do it"
                    }
                }
            },
            "inputs": { "seed": "x" }
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse flow.start");
        assert_eq!(command.name(), "flow.start");
        assert_eq!(command.tool_name(), None);
        match command {
            RuntimeCommand::FlowStart {
                workflow,
                inputs,
                workspace,
            } => {
                match workflow {
                    FlowSource::Inline { workflow } => assert_eq!(workflow.name, "fan"),
                    FlowSource::Named { .. } => panic!("expected inline workflow"),
                }
                assert_eq!(inputs.unwrap().get("seed").map(String::as_str), Some("x"));
                assert_eq!(workspace, None);
            }
            other => panic!("unexpected variant: {}", other.name()),
        }
        assert!(RUNTIME_COMMAND_NAMES.contains(&"flow.start"));
    }

    #[test]
    fn flow_start_round_trips_named_reference() {
        let value = serde_json::json!({
            "kind": "flow.start",
            "workflow_ref": "triage",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse flow.start ref");
        match command {
            RuntimeCommand::FlowStart { workflow, .. } => match workflow {
                FlowSource::Named { workflow_ref } => assert_eq!(workflow_ref, "triage"),
                FlowSource::Inline { .. } => panic!("expected named ref"),
            },
            other => panic!("unexpected variant: {}", other.name()),
        }
    }

    #[test]
    fn flow_steer_round_trips_with_and_without_target() {
        // Explicit node target round-trips and is on the wire.
        let value = serde_json::json!({
            "kind": "flow.steer",
            "flow_id": "f1",
            "target": { "node_id": "stage-1" },
            "message": "now run the tests",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse flow.steer");
        assert_eq!(command.name(), "flow.steer");
        assert_eq!(command.tool_name(), None);
        match &command {
            RuntimeCommand::FlowSteer {
                flow_id,
                target,
                message,
            } => {
                assert_eq!(flow_id, "f1");
                assert_eq!(target, &WorkerSelector::node("stage-1"));
                assert_eq!(message, "now run the tests");
            }
            other => panic!("unexpected: {}", other.name()),
        }
        // An omitted target defaults to "the only live worker" and is skipped on
        // the wire (so the default never bloats the command value).
        let bare: RuntimeCommand = serde_json::from_value(serde_json::json!({
            "kind": "flow.steer",
            "flow_id": "f1",
            "message": "go",
        }))
        .expect("parse bare flow.steer");
        match &bare {
            RuntimeCommand::FlowSteer { target, .. } => {
                assert!(target.is_default());
                assert_eq!(target.node_id, None);
            }
            other => panic!("unexpected: {}", other.name()),
        }
        let json = serde_json::to_value(&bare).expect("serialize bare");
        assert!(
            json.get("target").is_none(),
            "default target is skipped: {json}"
        );
        assert!(RUNTIME_COMMAND_NAMES.contains(&"flow.steer"));
    }

    #[test]
    fn flow_replay_round_trips_by_flow_id_and_by_path() {
        // The common case: replay a recorded flow by id (untagged → FlowId arm).
        let value = serde_json::json!({
            "kind": "flow.replay",
            "flow_id": "job-7",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse flow.replay id");
        assert_eq!(command.name(), "flow.replay");
        assert_eq!(command.tool_name(), None);
        match &command {
            RuntimeCommand::FlowReplay {
                ledger_ref,
                workspace,
            } => {
                assert_eq!(
                    ledger_ref,
                    &LedgerRef::FlowId {
                        flow_id: "job-7".into()
                    }
                );
                assert_eq!(workspace, &None);
            }
            other => panic!("unexpected: {}", other.name()),
        }

        // The explicit-path arm round-trips too.
        let by_path: RuntimeCommand = serde_json::from_value(serde_json::json!({
            "kind": "flow.replay",
            "ledger_path": "/p/.nerve/flows/job-7/ledger.jsonl",
        }))
        .expect("parse flow.replay path");
        match by_path {
            RuntimeCommand::FlowReplay { ledger_ref, .. } => assert_eq!(
                ledger_ref,
                LedgerRef::Path {
                    ledger_path: "/p/.nerve/flows/job-7/ledger.jsonl".into()
                }
            ),
            other => panic!("unexpected: {}", other.name()),
        }
        assert!(RUNTIME_COMMAND_NAMES.contains(&"flow.replay"));
    }

    #[test]
    fn flow_get_list_close_round_trip() {
        let get: RuntimeCommand =
            serde_json::from_value(serde_json::json!({ "kind": "flow.get", "flow_id": "f1" }))
                .expect("parse flow.get");
        assert_eq!(get.name(), "flow.get");
        match get {
            RuntimeCommand::FlowGet { flow_id } => assert_eq!(flow_id, "f1"),
            other => panic!("unexpected: {}", other.name()),
        }

        let list: RuntimeCommand =
            serde_json::from_value(serde_json::json!({ "kind": "flow.list" }))
                .expect("parse flow.list");
        assert_eq!(list.name(), "flow.list");
        assert!(matches!(list, RuntimeCommand::FlowList));

        let close: RuntimeCommand =
            serde_json::from_value(serde_json::json!({ "kind": "flow.close", "flow_id": "f1" }))
                .expect("parse flow.close");
        assert_eq!(close.name(), "flow.close");
        match close {
            RuntimeCommand::FlowClose { flow_id } => assert_eq!(flow_id, "f1"),
            other => panic!("unexpected: {}", other.name()),
        }
        for name in ["flow.get", "flow.list", "flow.close"] {
            assert!(RUNTIME_COMMAND_NAMES.contains(&name));
        }
    }

    #[test]
    fn flow_respond_reuses_session_approval_decision() {
        let value = serde_json::json!({
            "kind": "flow.respond",
            "flow_id": "f1",
            "request_id": "approval-3",
            "decision": "allow_always",
        });
        let command: RuntimeCommand = serde_json::from_value(value).expect("parse flow.respond");
        assert_eq!(command.name(), "flow.respond");
        match command {
            RuntimeCommand::FlowRespond {
                flow_id,
                request_id,
                decision,
            } => {
                assert_eq!(flow_id, "f1");
                assert_eq!(request_id, "approval-3");
                assert_eq!(decision, SessionApprovalDecision::AllowAlways);
            }
            other => panic!("unexpected: {}", other.name()),
        }
        assert!(RUNTIME_COMMAND_NAMES.contains(&"flow.respond"));
    }

    #[test]
    fn approval_decision_helpers_and_serde() {
        use SessionApprovalDecision::*;
        assert!(Allow.allows() && AllowAlways.allows());
        assert!(!Deny.allows() && !DenyAlways.allows());
        assert!(AllowAlways.remember() && DenyAlways.remember());
        assert!(!Allow.remember() && !Deny.remember());
        assert_eq!(
            serde_json::to_value(AllowAlways).unwrap(),
            serde_json::json!("allow_always")
        );
        assert_eq!(
            serde_json::to_value(DenyAlways).unwrap(),
            serde_json::json!("deny_always")
        );
    }
}
