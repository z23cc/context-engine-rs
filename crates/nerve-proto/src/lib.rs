//! Transport-neutral runtime-protocol vocabulary — the shared, wasm-safe data
//! types that define the Nerve runtime protocol (Protocol v4).
//!
//! This crate is the **single protocol authority**: the `RuntimeCommand` /
//! `RuntimeEvent` families, the declarative `flow.*` workflow types, the advisory
//! `RiskTier` / `ToolCapability` descriptors, the job/info/response payloads, and
//! the protocol version + method constants. Everything here is pure `serde` data
//! with **no dependency on `nerve-core`** (and therefore no tree-sitter / C
//! grammars), so it compiles to `wasm32-unknown-unknown` — a future Leptos WASM
//! frontend depends on this crate to share the EXACT protocol types with the
//! engine, with no codegen/TS drift.
//!
//! `nerve-runtime` re-exports this crate's surface unchanged, so the engine-side
//! paths (`nerve_runtime::RuntimeCommand`, `nerve_runtime::protocol::RuntimeInfo`,
//! …) keep resolving. The `#[derive(JsonSchema)]`s are gated behind the `schema`
//! feature (off by default) so the WASM frontend can omit `schemars`; the export
//! bin and `nerve-runtime` enable it.

pub mod command;
pub mod event;
pub mod flow;
pub mod job;
pub mod ledger;
pub mod outcome;
pub mod policy;
pub mod protocol;
#[doc(hidden)]
pub mod protocol_codegen;
pub mod provenance;
pub mod receipt;
pub mod risk;
pub mod tool_spec;
pub mod verdict;

pub use command::{
    ApprovalMode, AuthStartFlow, DelegateAutonomy, DelegateRole, FlowSource, LedgerRef, OtelSource,
    RUNTIME_COMMAND_NAMES, RuntimeCommand, SessionApprovalDecision, WorkerSelector,
};
pub use event::{
    AgentEventKind, AuthEventKind, FlowDecisionKind, FlowNodeUsage, FlowRunOutcome, FlowWorkerKind,
    RuntimeEvent, WechatEventKind,
};
pub use flow::{
    BudgetSpec, ContextSplit, FailPolicy, Join, Step, Strategy, TaskTemplate, WorkerRef,
    WorkflowDef,
};
pub use job::{
    RuntimeJobCancelRequest, RuntimeJobError, RuntimeJobGetRequest, RuntimeJobListRequest,
    RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
};
pub use ledger::{
    AdvisoryJudge, LEDGER_SCHEMA_VERSION, LedgerHead, LedgerKind, LedgerRecord,
    PolicyDecisionOutcome,
};
pub use outcome::{
    LabelSource, OUTCOME_SCHEMA_VERSION, Outcome, OutcomeLabel, OutcomeRecord, OutcomeSummary,
};
pub use policy::{
    Capability, CapabilityRule, EvidenceRequirement, MergeBar, POLICY_SCHEMA_VERSION,
    PolicyDecisionRecord, PolicyDoc,
};
pub use protocol::{HostCapabilities, HostCapabilitySupport};
pub use provenance::{Event, EventKind, LedgerEntry, RUN_SCHEMA_VERSION, Run};
pub use receipt::{
    LedgerRef as ReceiptLedgerRef, RECEIPT_PREDICATE_TYPE, RECEIPT_SCHEMA_VERSION, Receipt,
    ReceiptCheck, ReceiptProvenance, ReceiptSignature, ReceiptStatement,
    ReplayManifest as ReceiptReplayManifest,
};
pub use risk::{RiskTier, ToolCapability};
pub use tool_spec::RuntimeToolSpec;
pub use verdict::{
    CheckKind, CheckResult, CheckStatus, VERDICT_SCHEMA_VERSION, Verdict, VerdictStatus,
};
