use crate::{
    ApprovalMode, RUNTIME_COMMAND_NAMES, RuntimeCommand, RuntimeEvent, RuntimeJobCancelRequest,
    RuntimeJobError, RuntimeJobGetRequest, RuntimeJobListRequest, RuntimeJobSnapshot,
    RuntimeJobStartRequest, RuntimeToolSpec,
};
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RUNTIME_PROTOCOL_NAME: &str = "nerve-runtime";
// v4 (C2): additive `flow.*` command family + `flow_*` events (the Conductor,
// agent-orchestration design §4). All additions are new serde-tagged variants
// reusing AgentEventKind / SessionApprovalDecision / ApprovalRequested — no
// broken or removed fields, so a v3 client keeps working.
pub const RUNTIME_PROTOCOL_VERSION: &str = "4";
pub const RUNTIME_DAEMON_NAME: &str = "nerve";
pub const RUNTIME_EVENT_METHOD: &str = "runtime/event";
pub const RUNTIME_INFO_METHOD: &str = "runtime/info";
pub const RUNTIME_TOOLS_LIST_METHOD: &str = "runtime/tools/list";
pub const RUNTIME_JOB_START_METHOD: &str = "runtime/jobs/start";
pub const RUNTIME_JOB_GET_METHOD: &str = "runtime/jobs/get";
pub const RUNTIME_JOB_LIST_METHOD: &str = "runtime/jobs/list";
pub const RUNTIME_JOB_CANCEL_METHOD: &str = "runtime/jobs/cancel";
pub const RUNTIME_JOB_METHODS: &[&str] = &[
    RUNTIME_JOB_START_METHOD,
    RUNTIME_JOB_GET_METHOD,
    RUNTIME_JOB_LIST_METHOD,
    RUNTIME_JOB_CANCEL_METHOD,
];

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub protocol: String,
    pub protocol_version: String,
    pub server_info: RuntimeServerInfo,
    pub capabilities: RuntimeCapabilities,
}

impl RuntimeInfo {
    #[must_use]
    pub fn current(server_name: impl Into<String>, server_version: impl Into<String>) -> Self {
        Self {
            protocol: RUNTIME_PROTOCOL_NAME.to_string(),
            protocol_version: RUNTIME_PROTOCOL_VERSION.to_string(),
            server_info: RuntimeServerInfo {
                name: server_name.into(),
                version: server_version.into(),
            },
            capabilities: RuntimeCapabilities::current(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeCapabilities {
    pub transport: RuntimeTransportCapabilities,
    pub events: RuntimeEventCapabilities,
    pub jobs: RuntimeJobCapabilities,
}

/// Host-shell capabilities available to GUI/runtime clients through protocol
/// commands. These are intentionally concrete native affordances, not product
/// wishes: clients should only render native integration affordances when the
/// host reports support here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct HostCapabilities {
    pub host: String,
    pub platform: String,
    pub workspace_reveal: bool,
    pub native_window_chrome: bool,
    pub native_settings_window: bool,
    pub native_file_dialogs: bool,
    pub global_hotkey: bool,
    pub native_drag_drop: bool,
    pub os_notifications: bool,
    #[serde(default)]
    pub external_url_open: bool,
    pub clipboard_write_text: bool,
    pub rich_clipboard: bool,
    pub native_context_menu: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_color_scheme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_accent_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_accent_ink_color: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostCapabilitySupport {
    pub clipboard_write_text: bool,
    pub os_notifications: bool,
    pub native_file_dialogs: bool,
    pub external_url_open: bool,
    pub system_color_scheme: Option<String>,
    pub system_accent_color: Option<String>,
    pub system_accent_ink_color: Option<String>,
}

impl HostCapabilities {
    #[must_use]
    pub fn daemon_web(platform: impl Into<String>, support: HostCapabilitySupport) -> Self {
        Self {
            host: "nerve-daemon".to_string(),
            platform: platform.into(),
            workspace_reveal: true,
            native_window_chrome: false,
            native_settings_window: false,
            native_file_dialogs: support.native_file_dialogs,
            global_hotkey: false,
            native_drag_drop: false,
            os_notifications: support.os_notifications,
            external_url_open: support.external_url_open,
            clipboard_write_text: support.clipboard_write_text,
            rich_clipboard: false,
            native_context_menu: false,
            system_color_scheme: support.system_color_scheme,
            system_accent_color: support.system_accent_color,
            system_accent_ink_color: support.system_accent_ink_color,
        }
    }
}

impl RuntimeCapabilities {
    #[must_use]
    pub fn current() -> Self {
        Self {
            transport: RuntimeTransportCapabilities::default(),
            events: RuntimeEventCapabilities::default(),
            jobs: RuntimeJobCapabilities::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeTransportCapabilities {
    pub jsonrpc: String,
    pub framing: String,
}

impl Default for RuntimeTransportCapabilities {
    fn default() -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            framing: "ndjson".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeEventCapabilities {
    pub method: String,
}

impl Default for RuntimeEventCapabilities {
    fn default() -> Self {
        Self {
            method: RUNTIME_EVENT_METHOD.to_string(),
        }
    }
}

/// Params payload of a `runtime/event` notification: the event itself plus a
/// monotonic, per-stream sequence number so clients can detect dropped events
/// and request replay. The event fields are flattened, so this is backward
/// compatible with clients that read the bare event from `params`; `event_seq`
/// is an additive sibling field (defaulting to 0 until a later wave assigns the
/// real monotonic value at emit time).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeEventNotification {
    /// Monotonically increasing, gap-detectable sequence number for this stream.
    #[serde(default)]
    pub event_seq: u64,
    #[serde(flatten)]
    pub event: RuntimeEvent,
}

impl RuntimeEventNotification {
    #[must_use]
    pub fn new(event_seq: u64, event: RuntimeEvent) -> Self {
        Self { event_seq, event }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeJobCapabilities {
    pub methods: Vec<String>,
    pub command_kinds: Vec<String>,
}

impl Default for RuntimeJobCapabilities {
    fn default() -> Self {
        Self {
            methods: RUNTIME_JOB_METHODS
                .iter()
                .map(ToString::to_string)
                .collect(),
            command_kinds: RUNTIME_COMMAND_NAMES
                .iter()
                .map(ToString::to_string)
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeToolsListResponse {
    pub tools: Vec<RuntimeToolSpec>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeJobResponse {
    pub job: RuntimeJobSnapshot,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeJobListResponse {
    pub jobs: Vec<RuntimeJobSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobCancelResponse {
    pub cancellation_requested: bool,
    pub job: RuntimeJobSnapshot,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProtocolSchema {
    pub json_value: serde_json::Value,
    pub runtime_command: RuntimeCommand,
    pub approval_mode: ApprovalMode,
    pub runtime_event: RuntimeEvent,
    pub runtime_event_notification: RuntimeEventNotification,
    pub runtime_info: RuntimeInfo,
    pub host_capabilities: HostCapabilities,
    pub runtime_tool_spec: RuntimeToolSpec,
    pub runtime_job_error: RuntimeJobError,
    pub runtime_job: RuntimeJobSnapshot,
    pub runtime_job_start_request: RuntimeJobStartRequest,
    pub runtime_job_get_request: RuntimeJobGetRequest,
    pub runtime_job_list_request: RuntimeJobListRequest,
    pub runtime_job_cancel_request: RuntimeJobCancelRequest,
    pub runtime_tools_list_response: RuntimeToolsListResponse,
    pub runtime_job_response: RuntimeJobResponse,
    pub runtime_job_list_response: RuntimeJobListResponse,
    pub runtime_job_cancel_response: RuntimeJobCancelResponse,
}
