//! C0: a MINIMAL append-only, seq-numbered worker tape.
//!
//! The orchestration design (`docs/designs/agent-orchestration.md` §5) specifies
//! ONE `WorkerLedger` serving four jobs at once — the replay tape, the cross-worker
//! blackboard, the persistence record, and the resume source. C0 builds only the
//! first slice: an append-only, seq-numbered record of every [`WorkerEvent`] and
//! [`TurnResult`], keyed by `node_id`, behind a [`Mutex`]. C1 extends it to the
//! full content-addressed tape + blackboard + `FlowStore` persistence; until then
//! this is just enough to prove the engine writes are serialized and ordered.
#![allow(
    dead_code,
    reason = "C0 worker port awaits its C1 engine caller (mirrors subagent::bounded_fan_out)"
)]

use super::{TurnResult, WorkerEvent};
use std::sync::Mutex;

/// One recorded tape entry: a monotonically-increasing `seq`, the `node_id` that
/// produced it, and the payload (an event or a turn result).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LedgerEntry {
    pub(crate) seq: u64,
    pub(crate) node_id: String,
    pub(crate) payload: LedgerPayload,
}

/// What a [`LedgerEntry`] records. C1 adds artifacts / blackboard outputs here.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum LedgerPayload {
    /// A streamed [`WorkerEvent`] from a worker.
    Event(WorkerEvent),
    /// A node's final [`TurnResult`].
    Result(TurnResult),
}

/// The append-only worker tape. Only the engine writes; writes are serialized
/// through the [`Mutex`] so the tape order is deterministic and replayable.
#[derive(Default)]
pub(crate) struct WorkerLedger {
    entries: Mutex<Vec<LedgerEntry>>,
}

impl WorkerLedger {
    /// A fresh, empty ledger.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append a streamed event for `node_id`, returning its assigned seq number.
    pub(crate) fn record_event(&self, node_id: &str, event: WorkerEvent) -> u64 {
        self.append(node_id, LedgerPayload::Event(event))
    }

    /// Append a node's final result, returning its assigned seq number.
    pub(crate) fn record_result(&self, node_id: &str, result: &TurnResult) -> u64 {
        self.append(node_id, LedgerPayload::Result(result.clone()))
    }

    /// The current tape (a cloned snapshot), in seq order. For tests + the future
    /// engine's fold; cheap because C0 tapes are short.
    #[must_use]
    pub(crate) fn snapshot(&self) -> Vec<LedgerEntry> {
        crate::sync::lock_recover(&self.entries).clone()
    }

    /// The number of entries recorded so far.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        crate::sync::lock_recover(&self.entries).len()
    }

    fn append(&self, node_id: &str, payload: LedgerPayload) -> u64 {
        let mut entries = crate::sync::lock_recover(&self.entries);
        let seq = entries.len() as u64;
        entries.push(LedgerEntry {
            seq,
            node_id: node_id.to_string(),
            payload,
        });
        seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::AgentEventKind;

    fn step(text: &str) -> WorkerEvent {
        WorkerEvent::Step(AgentEventKind::Message { text: text.into() })
    }

    #[test]
    fn records_events_and_results_with_monotonic_seq() {
        let ledger = WorkerLedger::new();
        assert_eq!(ledger.record_event("n1", step("a")), 0);
        assert_eq!(ledger.record_event("n1", step("b")), 1);
        let result = TurnResult {
            ok: true,
            text: "done".into(),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        };
        assert_eq!(ledger.record_result("n1", &result), 2);
        assert_eq!(ledger.len(), 3);

        let tape = ledger.snapshot();
        assert_eq!(tape.iter().map(|e| e.seq).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(tape.iter().all(|e| e.node_id == "n1"));
        assert!(matches!(tape[0].payload, LedgerPayload::Event(_)));
        assert!(matches!(tape[2].payload, LedgerPayload::Result(_)));
    }

    #[test]
    fn entries_are_keyed_by_node_id() {
        let ledger = WorkerLedger::new();
        ledger.record_event("alpha", step("x"));
        ledger.record_event("beta", step("y"));
        let tape = ledger.snapshot();
        assert_eq!(tape[0].node_id, "alpha");
        assert_eq!(tape[1].node_id, "beta");
    }
}
