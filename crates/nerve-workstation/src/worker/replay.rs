//! The production [`ReplayWorker`] — REPLAY mode (design §3).
//!
//! A [`ReplayWorker`] re-emits a RECORDED node's events and result instead of
//! calling any LLM/subprocess: it reads a recorded [`WorkerLedger`] tape and, for
//! the node whose RENDERED prompt it is handed, replays that node's `Event` entries
//! (in recorded seq order) and recovers its `Result`. This is the engine running
//! offline over a tape — the audit moat (`flow.replay`, the byte-identical CI gate).
//!
//! ## Why keying by rendered prompt is faithful
//!
//! Replay runs the SAME deterministic engine over the SAME [`WorkflowDef`]; the
//! engine re-renders each node's task template against the SAME recorded blackboard
//! (the ledger's `Result` outputs, replayed in declared order), so the prompt a
//! replay worker receives is byte-identical to the one the record run produced. The
//! recorded `Start` entries give the prompt→node map directly
//! ([`WorkerLedger::prompt_to_node`]), so replay is SELF-CONTAINED from the persisted
//! tape — no out-of-band map.
//!
//! File mutations are RECORDED ARTIFACTS, not live FS reads (design §5): a replay
//! worker never touches the filesystem, so a node that mutated files during the
//! record run replays its recorded effect (events + result) honestly, and the pinned
//! per-node `snapshot_generation` is replayed from the tape by the driver's
//! generation provider — keeping the replay byte-identical even under mutation.

use super::{
    AgentWorker, LedgerEntry, LedgerPayload, TurnResult, WorkerContext, WorkerError, WorkerEvent,
    WorkerKind, WorkerSession,
};
use nerve_core::CancelToken;
use nerve_runtime::RiskTier;
use std::collections::BTreeMap;
use std::sync::Arc;

/// A worker that re-emits a recorded node's tape (design §3, REPLAY). Keyed by the
/// rendered prompt → node id recovered from the recorded `Start` entries, so each
/// replayed node re-emits exactly its own recorded events and returns its recorded
/// result — never calling an LLM/subprocess.
pub(crate) struct ReplayWorker {
    /// The recorded tape (immutable; shared across all replay workers in a run).
    recorded: Arc<Vec<LedgerEntry>>,
    /// rendered-prompt → node_id, recovered from the recorded `Start` entries.
    prompt_to_node: Arc<BTreeMap<String, String>>,
}

impl ReplayWorker {
    /// Build a replay worker over a shared recorded tape + prompt→node map.
    pub(crate) fn new(
        recorded: Arc<Vec<LedgerEntry>>,
        prompt_to_node: Arc<BTreeMap<String, String>>,
    ) -> Self {
        Self {
            recorded,
            prompt_to_node,
        }
    }
}

impl AgentWorker for ReplayWorker {
    fn kind(&self) -> WorkerKind {
        // Replay is worker-kind-agnostic; the recorded events already carry the
        // worker's behaviour. A stable label keeps the kind deterministic.
        WorkerKind::Cli("replay")
    }

    fn capability(&self) -> RiskTier {
        // A replay worker performs no live action, so it can reach nothing.
        RiskTier::ReadOnly
    }

    fn start(
        &self,
        task: &super::WorkerTask,
        _ctx: &WorkerContext,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let node = self
            .prompt_to_node
            .get(&task.prompt)
            .cloned()
            .ok_or_else(|| WorkerError::Start(format!("no recorded node for `{}`", task.prompt)))?;
        // Re-emit this node's recorded events, in recorded seq order, and recover its
        // recorded final result — never touching an LLM/process. The `Start` entry is
        // metadata (prompt + generation), not re-emitted.
        let mut last = TurnResult {
            ok: false,
            text: "replay: node had no recorded result".into(),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        };
        for entry in self.recorded.iter().filter(|e| e.node_id == node) {
            match &entry.payload {
                LedgerPayload::Event(event) => on_event(event.clone()),
                LedgerPayload::Result(result) => last = result.clone(),
                LedgerPayload::Start { .. } => {}
            }
        }
        Ok(Box::new(ReplaySession { last }))
    }
}

/// A replayed session: turn 1 already re-emitted in `start`. Steering is refused —
/// a replay re-runs only the recorded turns (recorded nondeterminism, §5), it never
/// invents a new one.
struct ReplaySession {
    last: TurnResult,
}

impl WorkerSession for ReplaySession {
    fn steer(
        &mut self,
        _message: &str,
        _cancel: &CancelToken,
        _on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        Err(WorkerError::NotSteerable)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}
