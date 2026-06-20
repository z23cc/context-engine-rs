//! Daemon execution of `flow.replay` (Wave C4) — the audit verb (design §3/§4).
//!
//! Split out of [`flow_job`](super) for the file-size convention. This loads a
//! recorded flow (def + ledger) from the [`FlowStore`] by [`LedgerRef`] and re-runs
//! the SAME deterministic engine in REPLAY mode: a [`ReplayResolver`] hands every
//! node a `ReplayWorker` that re-emits the recorded events/results instead of
//! touching an LLM/subprocess, so the re-emitted `flow_*` event stream is
//! byte-identical to the recorded run, at zero cost. No budget, no live approver, no
//! persistence (replay reads the tape, never writes a new one).

use super::observer::{FlowEventObserver, emit_strategy_edges};
use super::{EventEmitter, FlowDeps, LiveFlows, combined_cancel};
use crate::flow::{Driver, ReplayResolver, replay_generation_provider};
use crate::flow_store::FlowStore;
use crate::worker::{SteerRegistry, WorkerLedger};
use nerve_core::CancelToken;
use nerve_runtime::{LedgerRef, RuntimeError, RuntimeEvent, SessionApprovalDecision, WorkflowDef};
use serde_json::Value;
use std::sync::Arc;

/// Execute a `flow.replay` (design §3/§4): load the recorded def + ledger from the
/// [`FlowStore`], then re-run the engine in REPLAY mode and re-emit the `flow_*`
/// stream byte-identically. The `job_id` is the replayed `flow_id`.
pub(crate) fn run_flow_replay(
    job_id: &str,
    ledger_ref: LedgerRef,
    deps: &FlowDeps,
    flows: &LiveFlows,
    emit: &Arc<EventEmitter>,
    cancel: &CancelToken,
) -> Result<Value, RuntimeError> {
    let store = deps.store.as_ref().ok_or_else(|| {
        RuntimeError::adapter("flow.replay needs a persisted flow store (no .nerve/flows scope)")
    })?;
    let (def, recorded) = load_recorded_flow(store, &ledger_ref)?;
    // Register the replay under the job id so flow.get/list/close observe it like any
    // flow; a fresh ledger + steer registry (the replay records its own re-emitted
    // tape, and a replayed flow has no live frontier to steer).
    let replay_ledger = Arc::new(WorkerLedger::new());
    let steer = Arc::new(SteerRegistry::new());
    let registry_cancel =
        flows.register(job_id, &def, Arc::clone(&steer), Arc::clone(&replay_ledger));
    let run_cancel = combined_cancel(cancel, &registry_cancel);

    emit(RuntimeEvent::flow_started(job_id, def.strategy.clone()));
    emit_strategy_edges(job_id, &def.strategy, emit);

    let resolver = ReplayResolver::from_ledger(&recorded);
    let generation = replay_generation_provider(&recorded);
    let observer = FlowEventObserver::new(job_id.to_string(), Arc::clone(emit));
    let on_progress = |progress: crate::flow::FlowProgress| observer.on_progress(&progress);
    // A deny-all approver: a replay re-emits recorded events and never raises a live
    // approval (the recorded run already resolved them).
    let approver: Arc<dyn crate::delegate_proxy::DelegateApprover> = Arc::new(ReplayApprover);
    let driver = Driver::new(&resolver, Arc::clone(&replay_ledger), approver, None)
        .with_observer(&observer)
        .with_progress(&on_progress)
        .with_generation(&generation);
    let outcome = driver.run(&def, &run_cancel);

    super::finish_flow(
        job_id,
        &def,
        &outcome,
        flows,
        emit,
        cancel,
        &registry_cancel,
    )
}

/// Load the recorded `(WorkflowDef, WorkerLedger)` a `flow.replay` re-runs, from the
/// [`FlowStore`] by [`LedgerRef`]: a `FlowId` loads `def.json` + `ledger.jsonl` from
/// `.nerve/flows/<id>/`; a `Path` loads the explicit `ledger.jsonl` plus the sibling
/// `def.json` in its directory. A missing/corrupt record is a clear adapter error.
fn load_recorded_flow(
    store: &FlowStore,
    ledger_ref: &LedgerRef,
) -> Result<(WorkflowDef, WorkerLedger), RuntimeError> {
    match ledger_ref {
        LedgerRef::FlowId { flow_id } => {
            let def = store.load_def(flow_id).map_err(|err| {
                RuntimeError::adapter(format!("load flow `{flow_id}` def: {err}"))
            })?;
            let ledger = store.load_ledger(flow_id).map_err(|err| {
                RuntimeError::adapter(format!("load flow `{flow_id}` ledger: {err}"))
            })?;
            Ok((def, ledger))
        }
        LedgerRef::Path { ledger_path } => load_recorded_from_path(ledger_path),
    }
}

/// Load a `(WorkflowDef, WorkerLedger)` from an explicit `ledger.jsonl` path: parse the
/// ledger, then the sibling `def.json` in the same directory (the FlowStore layout).
fn load_recorded_from_path(ledger_path: &str) -> Result<(WorkflowDef, WorkerLedger), RuntimeError> {
    let path = std::path::Path::new(ledger_path);
    let jsonl = std::fs::read_to_string(path)
        .map_err(|err| RuntimeError::adapter(format!("read ledger `{ledger_path}`: {err}")))?;
    let ledger = WorkerLedger::from_jsonl(&jsonl)
        .map_err(|err| RuntimeError::adapter(format!("parse ledger `{ledger_path}`: {err}")))?;
    let def_path = path
        .parent()
        .map(|dir| dir.join("def.json"))
        .ok_or_else(|| {
            RuntimeError::adapter(format!("ledger path `{ledger_path}` has no parent"))
        })?;
    let def_raw = std::fs::read_to_string(&def_path).map_err(|err| {
        RuntimeError::adapter(format!("read def `{}`: {err}", def_path.display()))
    })?;
    let def: WorkflowDef = serde_json::from_str(&def_raw).map_err(|err| {
        RuntimeError::adapter(format!("parse def `{}`: {err}", def_path.display()))
    })?;
    Ok((def, ledger))
}

/// The approver a `flow.replay` runs under: a replay re-emits recorded events and
/// never raises a live approval, so this is a deny-all sentinel that is never consulted.
struct ReplayApprover;

impl crate::delegate_proxy::DelegateApprover for ReplayApprover {
    fn request(
        &self,
        _session_id: &str,
        _tool: &str,
        _args: &Value,
        _tier: nerve_runtime::RiskTier,
        _preview: String,
        _cancel: &CancelToken,
    ) -> SessionApprovalDecision {
        SessionApprovalDecision::Deny
    }
}
