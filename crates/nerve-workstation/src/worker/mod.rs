//! C0 (keystone): the unified `AgentWorker` host port.
//!
//! Two unrelated halves drive AI work in this binary today: the external-CLI
//! delegation path ([`delegate_live`](crate::delegate_live) + the persistent
//! `DelegateSession`/`CodexSession` drivers) and the in-process provider path
//! ([`subagent`](crate::subagent)'s `SubAgentSpawner::run_at_depth` over an
//! `Orchestrator`). This module introduces the single missing abstraction the
//! orchestration design (`docs/designs/agent-orchestration.md` §2) calls for: a
//! **lifecycle** worker port that BOTH halves implement, so the future conductor
//! engine (C1) sees only `AgentWorker` and is worker-kind-agnostic.
//!
//! ## Additive by construction
//!
//! C0 is a NEW module that *wraps* the shipped mechanisms — it does not modify the
//! `delegate_*` / `subagent` / `agent` code paths. [`CliWorker`] reuses the
//! persistent delegate drivers ([`DelegateSession`](crate::delegate_session)/
//! [`CodexSession`](crate::delegate_session_codex)) and the gemini one-shot
//! launcher recipe; [`ProviderWorker`] reuses `SubAgentSpawner::run_at_depth`
//! under the SAME outermost `PolicyToolBox` gate. Every existing test stays green
//! without edits.
//!
//! ## The property the port buys (design §2)
//!
//! Both families emit the **same `WorkerEvent` stream** (`Step` of the existing
//! [`nerve_runtime::AgentEventKind`], plus a raw `Progress` line for opaque CLIs)
//! and raise approvals through the **same** [`DelegateApprover`](crate::delegate_proxy::DelegateApprover)
//! hub. So the engine, the [`WorkerLedger`], and every client are kind-agnostic —
//! [`WorkerKind`] is the only place the CLI-vs-provider distinction is visible.
//!
//! ## What is stubbed for C1 (honest partials)
//!
//! - [`BudgetGrant`] is a minimal placeholder: its fields are recorded but **not
//!   enforced** here. C3 wires the `FleetBudget` debit/cancel loop.
//! - [`WorkerLedger`] is a minimal append-only seq-numbered tape (events +
//!   results). C1 extends it to the full replay-tape / blackboard / persistence
//!   record (design §5).
//! - The [`WorkerFactory`] holds the shared deps and mints workers, but C0 does
//!   NOT rewrite `delegate_agent` / `spawn_agent` to call it (additive-only).
//!
//! C0 is additive: the port + workers exist and are tested, but the C1 engine is
//! their first production caller, so the public surface is dead until C1 lands.
#![allow(
    dead_code,
    unused_imports,
    reason = "C0 worker port awaits its C1 engine caller (mirrors subagent::bounded_fan_out)"
)]

mod cli;
mod factory;
mod ledger;
#[cfg(test)]
mod parity;
mod provider;

pub(crate) use cli::CliWorker;
pub(crate) use factory::WorkerFactory;
pub(crate) use ledger::{LedgerEntry, LedgerPayload, WorkerLedger};
pub(crate) use provider::ProviderWorker;

use nerve_core::CancelToken;
use nerve_runtime::{AgentEventKind, DelegateAutonomy, RiskTier};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

/// What a worker is, and the only place the CLI-vs-provider distinction is visible
/// to the engine (design §2). `Cli` names a delegate agent (`codex`/`claude`/
/// `gemini`); `Provider` names an in-process `LlmProvider` + model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkerKind {
    /// An external agentic CLI driven over the delegate substrate.
    Cli(&'static str),
    /// An in-process provider loop (`Orchestrator` over the Nerve tool surface).
    Provider { provider: String, model: String },
}

/// A budget grant carved from the fleet budget (design §6). **Not yet enforced**
/// in C0 — the fields are recorded and threaded through, but the debit/cancel loop
/// lands in C3 (`FleetBudget`). Kept here so the [`WorkerTask`] shape is stable for
/// the engine that C1 builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct BudgetGrant {
    /// Cost ceiling for this worker in USD, when the fleet caps it. `None` = no cap.
    pub(crate) max_cost_usd: Option<f64>,
    /// Token ceiling for this worker, when the fleet caps it. `None` = no cap.
    pub(crate) max_tokens: Option<u64>,
}

/// One unit of work handed to a worker. Reuses [`DelegateAutonomy`] verbatim — it
/// maps to the CLI sandbox flag (`delegate_runtime` recipes) or the provider's
/// exec capability — so no new autonomy vocabulary is minted.
#[derive(Debug, Clone)]
pub(crate) struct WorkerTask {
    pub(crate) prompt: String,
    pub(crate) autonomy: DelegateAutonomy,
    pub(crate) model: Option<String>,
    pub(crate) tool_filter: Option<Vec<String>>,
    /// Carved from the fleet budget (§6). Recorded, **not enforced** in C0.
    pub(crate) budget: BudgetGrant,
}

/// The streamed unit. Reuses the existing [`AgentEventKind`] plus a raw `Progress`
/// line for opaque CLIs (so NO new step vocabulary is minted), and an `Approval`
/// projection of a routed permission ask.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum WorkerEvent {
    /// A structured agent-loop step (TurnStarted / Message / Reasoning / Tool* /
    /// Interrupted / Usage), identical to what sessions and `agent.run` emit.
    Step(AgentEventKind),
    /// A raw stdout chunk from an opaque CLI (no structured projection).
    Progress(String),
    /// A permission ask routed through the ONE approval hub — re-projected so the
    /// engine/clients can render it kind-agnostically. Emitted alongside the
    /// existing hub round-trip, which still drives the operator decision.
    Approval {
        request_id: String,
        tool: String,
        args: serde_json::Value,
        tier: RiskTier,
        preview: String,
    },
}

/// The union of the shipped `DelegateOutcome` and the agent `RunOutcome`: the last
/// turn's structured result, with usage/cost. `usage` reuses [`nerve_agent::Usage`]
/// (input/output/cache tokens), which both substrates can produce.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TurnResult {
    pub(crate) ok: bool,
    pub(crate) text: String,
    pub(crate) usage: nerve_agent::Usage,
    pub(crate) cost_usd: Option<f64>,
    pub(crate) timed_out: bool,
}

impl TurnResult {
    /// A failed/empty result (no text, zero usage), for an error-mapped turn.
    fn failed() -> Self {
        Self {
            ok: false,
            text: String::new(),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        }
    }
}

/// Shared context for a worker run: the pinned root + snapshot generation, the
/// append-only [`WorkerLedger`], and the approval hub (the same
/// [`DelegateApprover`](crate::delegate_proxy::DelegateApprover) that
/// `session.respond` resolves against).
#[derive(Clone)]
pub(crate) struct WorkerContext {
    pub(crate) root: Option<PathBuf>,
    /// Pinned per node-start for replay fidelity (design §5). C0 records it but
    /// does not yet drive snapshot pinning — C4 wires that.
    pub(crate) snapshot_generation: u64,
    pub(crate) ledger: Arc<WorkerLedger>,
    pub(crate) approver: Arc<dyn crate::delegate_proxy::DelegateApprover>,
}

/// A worker-port failure (distinct from a turn that merely reported `ok=false`).
#[derive(Debug)]
pub(crate) enum WorkerError {
    /// The worker could not be started (spawn refused, provider unresolved, …).
    Start(String),
    /// A turn failed mid-flight (child died, stalled, provider error).
    Turn(String),
    /// The run was cancelled (the supplied [`CancelToken`] fired).
    Cancelled,
    /// This worker kind does not support steering (e.g. one-shot `gemini`).
    NotSteerable,
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(message) => write!(f, "worker start failed: {message}"),
            Self::Turn(message) => write!(f, "worker turn failed: {message}"),
            Self::Cancelled => write!(f, "worker run was cancelled"),
            Self::NotSteerable => write!(f, "this worker is one-shot and cannot be steered"),
        }
    }
}

/// The unified worker port (design §2): a lifecycle handle that starts a steerable
/// [`WorkerSession`]. `start` runs turn 1 and returns the live session; the engine
/// (C1) drives further turns via [`WorkerSession::steer`].
pub(crate) trait AgentWorker: Send + Sync {
    /// What this worker is (the only kind-visible distinction).
    fn kind(&self) -> WorkerKind;
    /// Worst-case risk tier this worker can reach (advisory, for the engine/gate).
    fn capability(&self) -> RiskTier;
    /// Start the worker on `task`, running turn 1 and streaming its events into
    /// `on_event`, then return the live session for further turns.
    fn start(
        &self,
        task: &WorkerTask,
        ctx: &WorkerContext,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError>;
}

/// A live, steerable worker session (design §2). Both substrates already model a
/// live session, which is why the port is lifecycle- rather than request/response-
/// shaped.
pub(crate) trait WorkerSession: Send {
    /// Run one more turn with `message`, streaming events into `on_event`.
    fn steer(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError>;
    /// Cooperatively interrupt the in-flight turn (fires the session cancel).
    fn interrupt(&self);
    /// Tear the session down (reap the child / drop retained state). Idempotent.
    fn close(&mut self);
    /// The last turn's structured result (usage/cost/text).
    fn result(&self) -> TurnResult;
}

/// Synthesize the canonical `Step` stream a finished turn implies, so an opaque CLI
/// turn emits the SAME shape (TurnStarted → Message → Usage) as the provider path.
/// Shared by [`CliWorker`] (which only sees raw progress + a final `TurnResult`)
/// so both families' streams are structurally comparable — the parity property.
pub(crate) fn synthesize_turn_steps(
    turn: u64,
    result: &TurnResult,
    on_event: &mut dyn FnMut(WorkerEvent),
) {
    on_event(WorkerEvent::Step(AgentEventKind::TurnStarted { turn }));
    if !result.text.is_empty() {
        on_event(WorkerEvent::Step(AgentEventKind::Message {
            text: result.text.clone(),
        }));
    }
    on_event(WorkerEvent::Step(usage_step(&result.usage)));
}

/// Map a [`nerve_agent::Usage`] to the protocol [`AgentEventKind::Usage`] step,
/// omitting zero cache counts (matching [`crate::agent_event`]'s discipline).
pub(crate) fn usage_step(usage: &nerve_agent::Usage) -> AgentEventKind {
    AgentEventKind::Usage {
        input_tokens: u64::from(usage.input_tokens),
        output_tokens: u64::from(usage.output_tokens),
        cache_read_tokens: (usage.cache_read_tokens > 0)
            .then(|| u64::from(usage.cache_read_tokens)),
        cache_creation_tokens: (usage.cache_creation_tokens > 0)
            .then(|| u64::from(usage.cache_creation_tokens)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_step_omits_zero_cache_counts() {
        let step = usage_step(&nerve_agent::Usage {
            input_tokens: 10,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_creation_tokens: 7,
        });
        match step {
            AgentEventKind::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => {
                assert_eq!(input_tokens, 10);
                assert_eq!(output_tokens, 4);
                assert_eq!(cache_read_tokens, None);
                assert_eq!(cache_creation_tokens, Some(7));
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn synthesize_turn_steps_emits_turn_message_usage_in_order() {
        let result = TurnResult {
            ok: true,
            text: "hello".into(),
            usage: nerve_agent::Usage {
                input_tokens: 3,
                output_tokens: 2,
                ..nerve_agent::Usage::default()
            },
            cost_usd: Some(0.01),
            timed_out: false,
        };
        let mut events = Vec::new();
        synthesize_turn_steps(1, &result, &mut |event| events.push(event));
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            WorkerEvent::Step(AgentEventKind::TurnStarted { turn: 1 })
        ));
        assert!(matches!(
            &events[1],
            WorkerEvent::Step(AgentEventKind::Message { text }) if text == "hello"
        ));
        assert!(matches!(
            events[2],
            WorkerEvent::Step(AgentEventKind::Usage { .. })
        ));
    }

    #[test]
    fn empty_text_turn_omits_the_message_step() {
        let result = TurnResult::failed();
        let mut events = Vec::new();
        synthesize_turn_steps(2, &result, &mut |event| events.push(event));
        // TurnStarted + Usage only (no empty Message).
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            WorkerEvent::Step(AgentEventKind::TurnStarted { turn: 2 })
        ));
        assert!(matches!(
            events[1],
            WorkerEvent::Step(AgentEventKind::Usage { .. })
        ));
    }
}
