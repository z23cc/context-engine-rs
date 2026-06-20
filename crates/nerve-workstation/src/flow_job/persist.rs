//! Durable flow persistence helpers (Wave C4, design §5).
//!
//! Split out of [`flow_job`](super) for the file-size convention. These wrap the
//! [`FlowStore`] writes a `flow.start` performs — the def + initial record at start,
//! the terminal record + final tape on finish — and build the live per-node
//! `snapshot_generation` provider. All best-effort: a store error is logged and
//! persistence is skipped, never failing the flow (persistence is durability, not
//! correctness).

use crate::flow::FlowOutcome;
use crate::flow_store::{FlowRecord, FlowStore};
use crate::tools::NerveRuntime;
use crate::worker::WorkerLedger;
use nerve_core::WorkspaceResolver;
use nerve_runtime::WorkflowDef;
use std::sync::Arc;

/// Build the live per-node `snapshot_generation` provider (design §5): each node-start
/// reads the workspace's current snapshot generation through the shared runtime's
/// resolver, so a re-snapshot between nodes bumps a later node's recorded generation.
/// `0` when no snapshot resolves (a rootless/empty workspace) — a stable, replayable
/// default. (Mid-flow re-snapshot bumping itself is a follow-on; the recorded-then-
/// replayed pin contract the byte-identical gate enforces holds regardless.)
pub(super) fn live_generation_provider(
    runtime: Arc<NerveRuntime>,
) -> impl Fn(&WorkflowDef, &str) -> u64 + Sync {
    move |_def: &WorkflowDef, _node: &str| {
        runtime
            .resolver()
            .resolve_workspace(None)
            .ok()
            .and_then(|provider| provider.snapshot_arc().ok())
            .map_or(0, |snapshot| snapshot.generation)
    }
}

/// Persist a flow's `def.json` + initial `record.json` at `flow.start`, returning the
/// in-flight [`FlowRecord`] to stamp on finish. Best-effort.
pub(super) fn persist_flow_start(
    store: Option<&FlowStore>,
    flow_id: &str,
    def: &WorkflowDef,
) -> FlowRecord {
    let record = FlowRecord::begin(flow_id, def);
    if let Some(store) = store {
        if let Err(err) = store.write_def(flow_id, def) {
            eprintln!("warning: failed to persist flow `{flow_id}` def: {err}");
        }
        if let Err(err) = store.write_record(&record) {
            eprintln!("warning: failed to persist flow `{flow_id}` record: {err}");
        }
    }
    record
}

/// Persist a flow's terminal record + final ledger tape on finish (C4). Best-effort.
pub(super) fn persist_flow_finish(
    store: Option<&FlowStore>,
    record: &mut FlowRecord,
    outcome: &FlowOutcome,
    ledger: &WorkerLedger,
) {
    let Some(store) = store else { return };
    record.finish(outcome.ok, outcome.summary.clone(), outcome.final_text());
    if let Err(err) = store.write_record(record) {
        eprintln!(
            "warning: failed to persist flow `{}` record: {err}",
            record.flow_id
        );
    }
    if let Err(err) = store.write_ledger(&record.flow_id, ledger) {
        eprintln!(
            "warning: failed to persist flow `{}` ledger: {err}",
            record.flow_id
        );
    }
}
