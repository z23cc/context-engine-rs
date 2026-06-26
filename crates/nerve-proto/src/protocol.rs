use crate::{
    ApprovalMode, RUNTIME_COMMAND_NAMES, RuntimeCommand, RuntimeEvent, RuntimeJobCancelRequest,
    RuntimeJobError, RuntimeJobGetRequest, RuntimeJobListRequest, RuntimeJobSnapshot,
    RuntimeJobStartRequest, RuntimeToolSpec,
};
#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RUNTIME_PROTOCOL_NAME: &str = "nerve-runtime";
// v16 (hermetic-replay-isolation brick (a) — the honest isolation tier, INV-R7): a new
// pure `IsolationTier` enum (`Unconfined < BestEffort < Contained[default] < Hermetic`)
// plus two additive fields stamping the PROBED containment fact of the launcher that
// actually ran — `RunInputs.isolation_tier` (the agent run) and
// `ReceiptProvenance.isolation_tier` (the L2 verify re-run, co-sealed INSIDE the signed
// statement so a verifier learns how hermetic the re-run was). Both are
// `#[serde(default, skip_serializing_if = "is_contained")]`, so the default/weak-honest
// `Contained` value is OMITTED on the wire: every pre-isolation run/receipt deserializes
// to `Contained` and re-serializes byte-identically — its `root_hash` / `receipt_id` are
// UNPERTURBED (additive-invariance). It closes the INV-R1 overclaim (a best-effort re-run
// was byte-indistinguishable from a hermetic one) BEFORE any kernel sandbox exists, and
// gives orgs the optional `nerve gate --require-isolation` downgrade-only floor. A v15
// client keeps working byte-for-byte.
// v13 (trust-substrate L1 lineage-by-content-address read facet): one additive optional
// filter field — `LedgerQuery.run_root_hash` — that makes wave-3's content-addressed
// lineage DAG traversable over the protocol/CLI/MCP: select every ledger record about one
// run by the run's content address (the `RunRecorded` whose `run_root_hash` this is, plus
// the `Verdict`/`ReceiptIssued` records that pin back to it via their `run_root_hash`),
// rather than by the mutable `run_id` string. `#[serde(default, skip_serializing_if =
// "Option::is_none")]`, so an existing `ledger.query` without it deserializes to None and
// re-serializes byte-identically — every other command/event and every prior ledger record
// are UNPERTURBED, and a v12 client keeps working byte-for-byte.
// v12 (trust-substrate L1 content-addressed lineage DAG): additive `Option` lineage
// edge fields on two existing `LedgerKind` arms — `Verdict.run_root_hash` (verdict→run)
// and `ReceiptIssued.{run_root_hash, verdict_id}` (receipt→run, receipt→verdict). Bound
// at the canonical seal, they tie verdict/receipt records to their run by the run's
// content address (and to the borrowed verdict by its content id) instead of by the
// mutable `run_id` string, so the §3 DAG (task→agent→diff→test-result→receipt) is
// tamper-evident. Each field is `#[serde(default, skip_serializing_if = "Option::is_none")]`,
// so an existing serialized record (without them) deserializes to None and re-serializes
// byte-identically — its `record_hash` and the whole L1 chain are UNPERTURBED, and a
// v11 client keeps working byte-for-byte.
// v11 (trust-substrate L6 calibration + L6→L1 linkage): two additive shapes. (1) The
// `CheckFlakyRate` schema root — the deterministic per-check flaky-rate signal
// (`permille(flaky_runs, runs)`, integer, no ML) — surfaced advisory on the
// `outcome.query` response as `flaky_rates`. (2) An `OutcomeRecorded` `LedgerKind` arm
// appended AFTER `ReceiptIssued` (byte-additive — existing variants unchanged), so a
// recorded outcome label mirrors onto the L1 evidence ledger as an OBSERVATION (INV-R1/
// R3/R4: never a verdict input). New schema-root + serde-tagged variant only; every
// existing command/event and every prior ledger record serialize byte-for-byte as
// before, so a v10 client keeps working.
// v10 (trust-substrate L1 closure): additive read-only `ledger.verify` command —
// re-derive the append-only evidence ledger via `nerve_core::ledger::verify_chain`
// and report `{ ok, count, head_hash }` (intact) or `{ ok:false, error, seq }`
// (tamper), making the L1 hash-chain's tamper-detection reachable over the protocol
// and the CLI. A single appended serde-tagged command variant; the previously-dead
// `ledger_appended` event now fires on every successful append. `ledger.query` and
// every existing command/event are RETAINED, so a v9 client keeps working byte-for-byte.
// v9 (trust-substrate live delegate granularity): additive `delegate_agent` event —
// the per-tool LIVE structured step of a delegated (external-CLI) run, reusing the
// existing `AgentEventKind` payload so GUI/TUI render per-tool rows instead of only the
// opaque `delegate_progress` text tail. A single appended serde-tagged event variant;
// `delegate_progress` is RETAINED, so a v8 client (and every existing run_id) keeps
// working byte-for-byte.
// v8 (trust-substrate L0 granularity): additive `tool_started` / `tool_finished`
// `EventKind`s on the run-capture tape — they index *which* tools an agent ran,
// files it edited, and commands it executed (lifted from claude `tool_use` /
// `tool_result` + codex `command_execution` / `file_change`). New serde-tagged enum
// variants appended after the pre-existing kinds only: a run using none of them
// serializes and content-addresses byte-for-byte as before, so a v7 client (and
// every existing run_id) keeps working.
// v6 (trust-substrate, L0 flight-recorder): additive read-only `run.list` /
// `run.get` commands + the `run_recorded` event + the `provenance` shapes
// (`Run` / `Event` / `EventKind` / `LedgerEntry`) reachable from the exported
// schema. New serde-tagged variants and new schema-roots only — no broken or
// removed fields, so a v5 client keeps working.
// v5 (trust-substrate, credibility floor): additive read-only `delegate.get` /
// `delegate.list` commands — enumerate/fetch the live external-agent (delegate)
// sessions the daemon is parking, so a cockpit can observe its whole fleet over
// the protocol instead of from a single client's local state. New serde-tagged
// variants only; a v4 client keeps working.
// v4 (C2): additive `flow.*` command family + `flow_*` events (the Conductor,
// agent-orchestration design §4). All additions are new serde-tagged variants
// reusing AgentEventKind / SessionApprovalDecision / ApprovalRequested — no
// broken or removed fields, so a v3 client keeps working.
// v7 (trust-substrate L0c–L6): additive replay.start / ledger.query / verify.* /
// policy.* / receipt.get / otel.ingest / outcome.* commands; replay_progress /
// replay_finished / ledger_appended / verification_completed / policy_decision_recorded /
// receipt_issued / gate_decided / run_ingested / outcome_labeled events; pinned
// RunInputs + Attestation on Run (RUN_SCHEMA_VERSION 1→2); and the verdict / ledger /
// policy / receipt / outcome schema roots. Additive serde-tagged variants and new
// schema-roots only — a v6 client keeps working.
// v15 (trust-substrate L3 checkspec-identity binding): two additive optional fields
// closing the v14 "by-name" trust gap (`docs/designs/frontier-l3-l6-sigstore.md` §1).
// (1) `ReceiptStatement.checkspec_hash` — the content address of the checkspec the
// receipt's checks were produced against (the receipt's copy of the sealed
// `Verdict.checkspec_hash`). (2) `MergeBar.expected_checkspec_hash` — the checkspec the
// org's bar was AUTHORED against. The gate now binds `required_checks` to that identity:
// a renamed/stubbed (`command:'true'`) check can no longer impersonate the org's real
// check by reusing its display name — a mismatch DOWNGRADES to neutral (INV-R1, never an
// upgrade). Both fields are `#[serde(default, skip_serializing_if = "Option::is_none")]`,
// so a receipt/bar without them serializes byte-identically to a v14 record
// (additive-invariance) — no receipt-id churn for existing receipts. A v14 client keeps
// working.
// v14 (trust-substrate L3 merge-bar enforcement): additive `merge_bar` +
// `required_evidence` fields on `ReceiptStatement` — the org's sealed bar is now
// CO-SEALED INTO (and signed as part of) the receipt statement so the merge gate
// enforces the bar the receipt SIGNED, never a host-side policy re-read (INV-R5).
// Both fields are `skip_serializing_if`-empty, so a receipt sealed without an org
// bar serializes byte-identically to a v13 receipt (additive-invariance) — no
// receipt-id churn for existing receipts. A v13 client keeps working.
pub const RUNTIME_PROTOCOL_VERSION: &str = "16";
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
    /// L0 provenance shape root: a `run.get` returns a [`crate::provenance::Run`],
    /// which transitively pulls `Event` / `EventKind` / `LedgerEntry` into the
    /// exported schema (they are not reachable from any wire field on their own —
    /// `run_recorded` carries only ids + the root hash).
    pub run: crate::provenance::Run,
    /// Trust-substrate L0c–L6 schema roots: shapes not otherwise reachable from a
    /// wire command/event field, surfaced so the exported schema documents them for
    /// third-party (offline) re-verification of receipts/ledger/verdicts.
    pub verdict: crate::verdict::Verdict,
    /// L6 advisory per-check flaky-rate shape — surfaced on the `outcome.query`
    /// response as `flaky_rates` (a JSON field, not a typed wire param), so it is not
    /// otherwise reachable from a command/event field. Observational only (INV-R1/R3).
    pub check_flaky_rate: crate::verdict::CheckFlakyRate,
    pub ledger_record: crate::ledger::LedgerRecord,
    pub ledger_head: crate::ledger::LedgerHead,
    pub policy_doc: crate::policy::PolicyDoc,
    pub policy_decision: crate::policy::PolicyDecisionRecord,
    pub receipt: crate::receipt::Receipt,
    pub outcome_record: crate::outcome::OutcomeRecord,
    pub outcome_summary: crate::outcome::OutcomeSummary,
    pub replay_manifest: crate::provenance::ReplayManifest,
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
