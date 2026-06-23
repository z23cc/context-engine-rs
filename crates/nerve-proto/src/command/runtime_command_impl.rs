use super::{AuthStartFlow, RuntimeCommand};

impl RuntimeCommand {
    /// Construct a browser-flow `auth.start` command with the wire-compatible default flow.
    #[must_use]
    pub fn auth_start(provider: impl Into<String>) -> Self {
        Self::auth_start_with_flow(provider, AuthStartFlow::Browser)
    }

    /// Construct an `auth.start` command with an explicit login flow.
    #[must_use]
    pub fn auth_start_with_flow(provider: impl Into<String>, flow: AuthStartFlow) -> Self {
        Self::AuthStart {
            provider: provider.into(),
            flow,
        }
    }

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
            Self::AuthLease { .. } => "auth.lease",
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
            Self::HostCapabilities => "host.capabilities",
            Self::HostClipboardWriteText { .. } => "host.clipboard.write_text",
            Self::HostNotificationShow { .. } => "host.notification.show",
            Self::HostFolderPick { .. } => "host.folder.pick",
            Self::HostFileSaveText { .. } => "host.file.save_text",
            Self::HostUrlOpen { .. } => "host.url.open",
            Self::WorkspaceReveal { .. } => "workspace.reveal",
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
            | Self::AuthLease { .. }
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
            | Self::FlowRespond { .. }
            | Self::HostCapabilities
            | Self::HostClipboardWriteText { .. }
            | Self::HostNotificationShow { .. }
            | Self::HostFolderPick { .. }
            | Self::HostFileSaveText { .. }
            | Self::HostUrlOpen { .. }
            | Self::WorkspaceReveal { .. } => None,
        }
    }
}
