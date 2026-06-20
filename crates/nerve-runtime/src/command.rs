use crate::RiskTier;
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
];

/// Transport-neutral command understood by human-facing runtime adapters.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
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
}

/// Autonomy posture handed to a delegated external agent CLI, mapping to each
/// vendor's sandbox/permission flag: codex `--sandbox`, claude `--permission-mode`,
/// gemini `--approval-mode` (read-only | edit | full). Defaults to the most
/// restricted ([`Self::ReadOnly`]) so an omitted field never grants more than read
/// access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
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
            | Self::DelegateClose { .. } => None,
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
            } => {
                assert_eq!(agent, "codex");
                assert_eq!(task, "add a test");
                assert_eq!(cwd, None);
                assert_eq!(autonomy, DelegateAutonomy::ReadOnly);
                assert_eq!(model, None);
            }
            other => panic!("unexpected variant: {}", other.name()),
        }
        assert!(RUNTIME_COMMAND_NAMES.contains(&"delegate.start"));
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
