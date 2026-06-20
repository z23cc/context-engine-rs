//! The `WorkerLedger` — one append-only structure serving THREE jobs at once.
//!
//! The orchestration design (`docs/designs/agent-orchestration.md` §5) specifies
//! ONE `WorkerLedger` that is simultaneously:
//!
//! 1. the **replay tape** — every [`WorkerEvent`] and [`TurnResult`] with a
//!    monotonically-increasing `seq`, so a recorded run can be re-emitted
//!    byte-identically (the determinism moat, design §3);
//! 2. the **cross-worker blackboard** — `node_id → output text`, the upstream
//!    results a downstream step interpolates into its task (design §3/§5);
//! 3. the **(persistence) record** — the whole tape serializes to JSONL, the
//!    on-disk shape `FlowStore` will persist (C4) and `flow.replay` will read.
//!
//! C1 builds all three in-memory; the fourth design job (cross-restart *resume*
//! from the persisted record) is C4. Only the engine writes, and every write is
//! serialized through one [`Mutex`] so the tape order is deterministic and
//! replayable.
#![allow(
    dead_code,
    reason = "C1 engine is the first caller; some ledger surface lands for C4 persistence/resume"
)]

use super::{TurnResult, WorkerEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// One recorded tape entry: a monotonically-increasing `seq`, the `node_id` that
/// produced it, and the payload (an event or a turn result).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct LedgerEntry {
    pub(crate) seq: u64,
    pub(crate) node_id: String,
    pub(crate) payload: LedgerPayload,
}

/// What a [`LedgerEntry`] records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "payload", rename_all = "snake_case")]
pub(crate) enum LedgerPayload {
    /// A node's start: the RENDERED prompt the worker received and the
    /// `snapshot_generation` pinned for that node (design §5). Recorded as the
    /// first entry for a node so replay is SELF-CONTAINED from the persisted tape:
    /// the rendered-prompt → node-id map and the per-node generation are both
    /// recoverable without re-deriving them. A node that mutated files makes a
    /// later node's generation differ, recorded honestly here.
    Start {
        prompt: String,
        snapshot_generation: u64,
    },
    /// A streamed [`WorkerEvent`] from a worker.
    Event(WorkerEvent),
    /// A node's final [`TurnResult`].
    Result(TurnResult),
}

/// The append-only worker tape + blackboard. Only the engine writes; writes are
/// serialized through the [`Mutex`] so the tape order is deterministic and
/// replayable. The blackboard is derived from `Result` entries (a node's final
/// text), kept as an index for O(1) downstream lookup.
#[derive(Default, Debug)]
pub(crate) struct WorkerLedger {
    inner: Mutex<LedgerInner>,
}

#[derive(Default, Debug)]
struct LedgerInner {
    /// Job 1: the replay tape (every event + result, in seq order).
    entries: Vec<LedgerEntry>,
    /// Job 2: the blackboard (node_id -> last recorded result text). A node that
    /// records multiple results keeps the latest, matching last-write-wins for a
    /// re-dispatched node.
    blackboard: BTreeMap<String, String>,
}

impl WorkerLedger {
    /// A fresh, empty ledger.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append a node-start entry: the RENDERED `prompt` the worker received and the
    /// pinned `snapshot_generation` (design §5). Recorded BEFORE the node's events
    /// so the tape is a self-contained replay source — the prompt→node map and the
    /// per-node generation are recoverable from the persisted ledger alone.
    pub(crate) fn record_start(
        &self,
        node_id: &str,
        prompt: &str,
        snapshot_generation: u64,
    ) -> u64 {
        self.append(
            node_id,
            LedgerPayload::Start {
                prompt: prompt.to_string(),
                snapshot_generation,
            },
        )
    }

    /// Append a streamed event for `node_id`, returning its assigned seq number.
    pub(crate) fn record_event(&self, node_id: &str, event: WorkerEvent) -> u64 {
        self.append(node_id, LedgerPayload::Event(event))
    }

    /// Append a node's final result, returning its assigned seq number. The
    /// result's text is also indexed into the blackboard under `node_id` for
    /// downstream interpolation (job 2).
    pub(crate) fn record_result(&self, node_id: &str, result: &TurnResult) -> u64 {
        let mut inner = crate::sync::lock_recover(&self.inner);
        inner
            .blackboard
            .insert(node_id.to_string(), result.text.clone());
        Self::push(&mut inner, node_id, LedgerPayload::Result(result.clone()))
    }

    /// Job 2 (blackboard): the recorded output text of `node_id`, if it finished.
    /// This is what a downstream step's [`TaskTemplate`](nerve_runtime::TaskTemplate)
    /// interpolates an upstream `{{node_id}}` placeholder from.
    #[must_use]
    pub(crate) fn output(&self, node_id: &str) -> Option<String> {
        crate::sync::lock_recover(&self.inner)
            .blackboard
            .get(node_id)
            .cloned()
    }

    /// The current tape (a cloned snapshot), in seq order. For tests, the
    /// engine's fold, and JSONL serialization.
    #[must_use]
    pub(crate) fn snapshot(&self) -> Vec<LedgerEntry> {
        crate::sync::lock_recover(&self.inner).entries.clone()
    }

    /// Job 3 (record): serialize the whole tape as JSONL (one entry per line) —
    /// the on-disk shape `FlowStore` persists (C4) and `flow.replay` reads. A
    /// trailing newline keeps the file append-friendly.
    #[must_use]
    pub(crate) fn to_jsonl(&self) -> String {
        let entries = self.snapshot();
        let mut out = String::new();
        for entry in &entries {
            // Entries are plain serde types, so serialization cannot fail; fall
            // back to an empty object rather than panicking on the off chance.
            let line = serde_json::to_string(entry).unwrap_or_else(|_| "{}".to_string());
            out.push_str(&line);
            out.push('\n');
        }
        out
    }

    /// Job 3 (record): reconstruct a ledger from JSONL (the inverse of
    /// [`Self::to_jsonl`]). Blank lines are skipped; a malformed line aborts with
    /// the parse error so a corrupt tape never silently replays wrong. The
    /// blackboard is rebuilt deterministically by re-folding the `Result` entries.
    pub(crate) fn from_jsonl(jsonl: &str) -> Result<Self, serde_json::Error> {
        let ledger = Self::new();
        let mut inner = crate::sync::lock_recover(&ledger.inner);
        for line in jsonl.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: LedgerEntry = serde_json::from_str(line)?;
            if let LedgerPayload::Result(result) = &entry.payload {
                inner
                    .blackboard
                    .insert(entry.node_id.clone(), result.text.clone());
            }
            inner.entries.push(entry);
        }
        drop(inner);
        Ok(ledger)
    }

    /// Recover the rendered-prompt → node-id map from the recorded `Start` entries
    /// (design §3, REPLAY): the production [`ReplayWorker`](super::ReplayWorker) keys
    /// off the rendered prompt a node received, which is deterministic — the engine
    /// re-renders the same templates against the same recorded blackboard, so the
    /// replayed prompt equals the recorded one. The LAST `Start` for a node wins
    /// (a re-dispatched node keeps its latest instruction), so resume-re-dispatch is
    /// honored. Returns `(prompt → node_id)`.
    #[must_use]
    pub(crate) fn prompt_to_node(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for entry in &crate::sync::lock_recover(&self.inner).entries {
            if let LedgerPayload::Start { prompt, .. } = &entry.payload {
                map.insert(prompt.clone(), entry.node_id.clone());
            }
        }
        map
    }

    /// Recover the per-node pinned `snapshot_generation` from the recorded `Start`
    /// entries (design §5, replay fidelity under file mutation). The LAST `Start`
    /// for a node wins. Returns `(node_id → snapshot_generation)`.
    #[must_use]
    pub(crate) fn node_generations(&self) -> BTreeMap<String, u64> {
        let mut map = BTreeMap::new();
        for entry in &crate::sync::lock_recover(&self.inner).entries {
            if let LedgerPayload::Start {
                snapshot_generation,
                ..
            } = &entry.payload
            {
                map.insert(entry.node_id.clone(), *snapshot_generation);
            }
        }
        map
    }

    /// The set of node ids that have a recorded `Result` (a finished node) — the
    /// resume "last node boundary" computation (design §5): a flow resumes by
    /// replaying these to rebuild scheduler + blackboard state, then scheduling the
    /// pending nodes live. Returns node ids in first-seen tape order.
    #[must_use]
    pub(crate) fn finished_nodes(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for entry in &crate::sync::lock_recover(&self.inner).entries {
            if matches!(entry.payload, LedgerPayload::Result(_)) && !seen.contains(&entry.node_id) {
                seen.push(entry.node_id.clone());
            }
        }
        seen
    }

    /// The number of entries recorded so far.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        crate::sync::lock_recover(&self.inner).entries.len()
    }

    /// Whether the tape is empty.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn append(&self, node_id: &str, payload: LedgerPayload) -> u64 {
        let mut inner = crate::sync::lock_recover(&self.inner);
        Self::push(&mut inner, node_id, payload)
    }

    fn push(inner: &mut LedgerInner, node_id: &str, payload: LedgerPayload) -> u64 {
        let seq = inner.entries.len() as u64;
        inner.entries.push(LedgerEntry {
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

    fn result(text: &str) -> TurnResult {
        TurnResult {
            ok: true,
            text: text.into(),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        }
    }

    #[test]
    fn records_events_and_results_with_monotonic_seq() {
        let ledger = WorkerLedger::new();
        assert_eq!(ledger.record_event("n1", step("a")), 0);
        assert_eq!(ledger.record_event("n1", step("b")), 1);
        assert_eq!(ledger.record_result("n1", &result("done")), 2);
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

    #[test]
    fn blackboard_indexes_node_result_text() {
        // Job 2: a downstream step reads an upstream node's output by node_id.
        let ledger = WorkerLedger::new();
        ledger.record_result("upstream", &result("the answer"));
        assert_eq!(ledger.output("upstream"), Some("the answer".to_string()));
        assert_eq!(ledger.output("missing"), None);
        // Last-write-wins for a re-dispatched node.
        ledger.record_result("upstream", &result("the better answer"));
        assert_eq!(
            ledger.output("upstream"),
            Some("the better answer".to_string())
        );
    }

    #[test]
    fn jsonl_round_trips_tape_and_rebuilds_blackboard() {
        // Job 3: serialize -> reconstruct yields the identical tape AND the
        // blackboard is re-folded deterministically.
        let ledger = WorkerLedger::new();
        ledger.record_event("n1", step("hello"));
        ledger.record_result("n1", &result("done"));
        ledger.record_result("n2", &result("also done"));

        let jsonl = ledger.to_jsonl();
        assert_eq!(jsonl.lines().count(), 3, "one line per entry");

        let restored = WorkerLedger::from_jsonl(&jsonl).expect("reconstruct from jsonl");
        assert_eq!(restored.snapshot(), ledger.snapshot(), "tape is identical");
        assert_eq!(restored.output("n1"), Some("done".to_string()));
        assert_eq!(restored.output("n2"), Some("also done".to_string()));
        // And serializing the reconstruction is byte-identical (stable form).
        assert_eq!(restored.to_jsonl(), jsonl);
    }

    #[test]
    fn every_worker_event_variant_round_trips_through_jsonl() {
        // Regression guard (C4): a `WorkerEvent` newtype variant cannot be carried by
        // the internally-tagged enum, which silently produced an unparseable `{}`
        // line in the ledger and broke replay. Pin that EVERY variant — a `Step`, a
        // `Progress`, and an `Approval` — survives the to_jsonl → from_jsonl round
        // trip byte-identically.
        let ledger = WorkerLedger::new();
        ledger.record_start("n1", "the prompt", 3);
        ledger.record_event(
            "n1",
            WorkerEvent::Step(AgentEventKind::TurnStarted { turn: 1 }),
        );
        ledger.record_event(
            "n1",
            WorkerEvent::Progress {
                text: "raw stdout line".into(),
            },
        );
        ledger.record_event(
            "n1",
            WorkerEvent::Approval {
                request_id: "req-1".into(),
                tool: "edit".into(),
                args: serde_json::json!({ "path": "a.rs" }),
                tier: nerve_runtime::RiskTier::Edit,
                preview: "diff".into(),
            },
        );
        ledger.record_result("n1", &result("done"));

        let jsonl = ledger.to_jsonl();
        // No silently-empty line (the failure mode the newtype Progress produced).
        assert!(
            !jsonl.lines().any(|l| l.trim() == "{}"),
            "a variant failed to serialize and fell back to `{{}}`: {jsonl}"
        );
        let restored = WorkerLedger::from_jsonl(&jsonl).expect("every variant round-trips");
        assert_eq!(restored.snapshot(), ledger.snapshot());
        assert_eq!(restored.to_jsonl(), jsonl);
        // The recovered prompt→node + generation maps are intact.
        assert_eq!(
            restored
                .prompt_to_node()
                .get("the prompt")
                .map(String::as_str),
            Some("n1")
        );
        assert_eq!(restored.node_generations().get("n1"), Some(&3));
    }

    #[test]
    fn from_jsonl_skips_blank_lines_and_errors_on_garbage() {
        let ledger = WorkerLedger::new();
        ledger.record_event("n1", step("x"));
        let mut jsonl = ledger.to_jsonl();
        jsonl.push('\n'); // a trailing blank line is tolerated
        let restored = WorkerLedger::from_jsonl(&jsonl).expect("blank lines skipped");
        assert_eq!(restored.len(), 1);

        WorkerLedger::from_jsonl("not json\n").expect_err("garbage aborts");
    }
}
