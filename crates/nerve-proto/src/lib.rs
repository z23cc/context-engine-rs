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
pub mod protocol;
#[doc(hidden)]
pub mod protocol_codegen;
pub mod risk;
pub mod tool_spec;

pub use command::{
    ApprovalMode, DelegateAutonomy, FlowSource, LedgerRef, RUNTIME_COMMAND_NAMES, RuntimeCommand,
    SessionApprovalDecision, WorkerSelector,
};
pub use event::{
    AgentEventKind, AuthEventKind, FlowDecisionKind, FlowNodeUsage, FlowRunOutcome, FlowWorkerKind,
    RuntimeEvent,
};
pub use flow::{
    BudgetSpec, ContextSplit, FailPolicy, Join, Step, Strategy, TaskTemplate, WorkerRef,
    WorkflowDef,
};
pub use job::{
    RuntimeJobCancelRequest, RuntimeJobError, RuntimeJobGetRequest, RuntimeJobListRequest,
    RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
};
pub use risk::{RiskTier, ToolCapability};
pub use tool_spec::RuntimeToolSpec;
