//! Cross-run **evidence ledger** vocabulary — the L1 append-only transparency log
//! (`docs/designs/trust-substrate.md` §3 L1, §8). Where the per-run
//! [`crate::provenance::LedgerEntry`] is the content-addressed *spine of one run*,
//! the [`LedgerRecord`] here is one node of the *cross-run* log that commits, in
//! order, to every trust-relevant fact the substrate observed: runs recorded,
//! diffs, policy decisions, verdicts, and issued receipts. The records form a
//! linear hash chain (`record_hash` over the record's identity, `prev_hash`
//! pointing at the predecessor) so any reader can re-derive the chain and confirm
//! the log is append-only and untampered (INV-R5).
//!
//! These are **pure, transport-neutral serde data** with **no behavior** — every
//! hash field is a plain `String`. The pure canonicalization + SHA-256 hashing
//! that *fills* the hash fields lives in `nerve-core::ledger` (INV-R2: hashing is
//! pure and golden-tested), never here. Hosts (the daemon) append and persist
//! records; `nerve-core` chains them; this crate only names the shapes so they are
//! wasm-shareable and appear in the exported protocol schema.
//!
//! **Court reporter, not judge (INV-R1).** Any LLM-derived label is *advisory*:
//! [`AdvisoryJudge`] is explicitly non-load-bearing and never gates a merge. The
//! load-bearing verdict on a [`LedgerKind::Verdict`] record reuses the canonical
//! [`crate::verdict`] types ([`crate::verdict::VerdictStatus`] /
//! [`crate::verdict::CheckResult`]) — the org's own execution-grounded checks.
//!
//! **No floats** appear in any hashed type: counts are `u64` and the advisory
//! confidence is integer basis-points (`u32`), so the canonical JSON is byte-stable
//! and the types derive `Eq` — exact golden snapshots, no precision nondeterminism.

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::outcome::{LabelSource, Outcome};
use crate::verdict::{CheckResult, VerdictStatus};

/// On-disk + on-wire ledger schema version. Bumped only for additive,
/// backward-compatible changes to the [`LedgerRecord`] / [`LedgerHead`] shapes
/// (mirrors [`crate::provenance::RUN_SCHEMA_VERSION`]); a reader rejects a record
/// from a newer major it cannot understand rather than silently dropping fields.
pub const LEDGER_SCHEMA_VERSION: u32 = 1;

/// The outcome of a recorded policy decision (`trust-substrate.md` §3 L3). A
/// two-valued allow/deny so the ledger's policy facts are golden-diffable; the
/// human-readable rationale lives in the decision record's `detail_hash`, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionOutcome {
    /// The capability request was permitted.
    Allow,
    /// The capability request was refused.
    Deny,
}

/// An **advisory**, explicitly non-load-bearing LLM-derived label attached to a
/// [`LedgerKind::Verdict`] record (INV-R1: *court reporter, not judge*). It never
/// gates a merge and never overrides the execution-grounded
/// [`crate::verdict::VerdictStatus`]; it is recorded only so the corpus can later
/// be calibrated against real outcomes. `confidence_bp` is integer basis-points
/// (0..=10000, i.e. hundredths of a percent) — never a float — so the record's
/// canonical bytes stay stable and the type derives `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct AdvisoryJudge {
    /// The advisory verdict label as the judge model named it (free-form).
    pub label: String,
    /// The judge's self-reported confidence in basis-points (0..=10000).
    pub confidence_bp: u32,
}

/// One trust-relevant fact recorded on the cross-run ledger (`trust-substrate.md`
/// §3 L1). Internally tagged (`{"kind": "...", ...}`), mirroring
/// [`crate::provenance::EventKind`] so the audit trail is golden-diffable. The set
/// is intentionally small and execution-grounded; additive — new kinds may be
/// appended. [`Self::Verdict`] reuses the canonical [`crate::verdict`] types rather
/// than redefining check/verdict shapes (single source of truth).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LedgerKind {
    /// A captured run was sealed and persisted. Binds the run's content address
    /// (`run_root_hash`), the agent, a hash of the task text, and the event count.
    RunRecorded {
        run_id: String,
        run_root_hash: String,
        agent: String,
        task_hash: String,
        event_count: u64,
    },
    /// A diff produced by a run was recorded, with its content hash and line stats.
    DiffRecorded {
        run_id: String,
        diff_hash: String,
        files: u64,
        added: u64,
        removed: u64,
    },
    /// A policy plane allow/deny decision was recorded. `detail_hash` (when present)
    /// commits to the decision's redacted rationale/args, kept out-of-band.
    PolicyDecision {
        run_id: String,
        policy_version: String,
        capability: String,
        decision: PolicyDecisionOutcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail_hash: Option<String>,
    },
    /// An execution-grounded verdict was reached for a run's diff. The load-bearing
    /// `verdict` and `checks` reuse the canonical [`crate::verdict`] types;
    /// `advisory_llm_judge`, when present, is non-load-bearing (INV-R1). `run_root_hash`,
    /// when present, is the content address of the captured run this verdict
    /// re-verified — the **verdict→run lineage edge** that binds the record to the run
    /// by content (not by the mutable `run_id` string), so the cross-run DAG is
    /// tamper-evident (`trust-substrate.md` §3 L1).
    Verdict {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_hash: Option<String>,
        verdict: VerdictStatus,
        checks: Vec<CheckResult>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        advisory_llm_judge: Option<AdvisoryJudge>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_root_hash: Option<String>,
    },
    /// A signed verification receipt was issued for a run. Binds the receipt id, the
    /// hash of the pinned inputs, the policy version in force, and the verdict it
    /// borrowed from the org's own checks. `run_root_hash` and `verdict_id`, when
    /// present, are the **receipt→run** and **receipt→verdict** lineage edges: the
    /// content address of the captured run and the content id of the [`Self::Verdict`]
    /// the receipt borrowed, so the DAG links task→agent→diff→test-result→receipt by
    /// content rather than by the mutable `run_id` string (`trust-substrate.md` §3 L1).
    ReceiptIssued {
        run_id: String,
        receipt_id: String,
        inputs_hash: String,
        policy_version: String,
        verdict: VerdictStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_root_hash: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verdict_id: Option<String>,
    },
    /// A human/CI/observation outcome label for a run was recorded (L6). Binds the
    /// run, the observed outcome + its source, and the content hash of the label.
    /// INV-R1/R3/R4: an OBSERVATION, never a verdict input.
    OutcomeRecorded {
        run_id: String,
        outcome: Outcome,
        source: LabelSource,
        label_hash: String,
    },
}

/// One node on the append-only cross-run ledger: a monotonic logical sequence
/// number, the typed fact, and the chain links. `record_hash` is the digest over
/// this record's identity (seq + kind); `prev_hash` is the predecessor's
/// `record_hash` (`""` for the first record) so a verifier re-derives the chain
/// from [`LedgerRecord`]s alone. `appended_at_ms` is host wall-clock metadata for
/// display and is **excluded from the hashed bytes**, so it never perturbs the
/// chain. `seq` is a logical clock (0,1,2,…) assigned at append, *not* a
/// wall-clock — so a replay reproduces byte-identical ordering and hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct LedgerRecord {
    pub schema_version: u32,
    pub seq: u64,
    pub kind: LedgerKind,
    pub record_hash: String,
    pub prev_hash: String,
    #[serde(default)]
    pub appended_at_ms: u64,
}

/// The current head of the cross-run ledger: the number of records appended and
/// the head record's `record_hash` (`""` for an empty log). Persisted alongside the
/// log so a host can append the next record without re-reading the whole chain; a
/// verifier confirms `count`/`head_hash` by re-deriving the chain.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct LedgerHead {
    pub schema_version: u32,
    pub count: u64,
    #[serde(default)]
    pub head_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::{LabelSource, Outcome};
    use crate::verdict::{CheckKind, CheckResult, CheckStatus, VerdictStatus};

    fn sample_check() -> CheckResult {
        CheckResult {
            name: "unit".into(),
            kind: CheckKind::Test,
            status: CheckStatus::Pass,
            reproducible: true,
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 12,
            output_hash: "ab".into(),
            runs: 1,
            passed: 1,
        }
    }

    #[test]
    fn policy_decision_outcome_tags_are_snake_case() {
        assert_eq!(
            serde_json::to_value(PolicyDecisionOutcome::Allow).expect("json"),
            serde_json::json!("allow")
        );
        assert_eq!(
            serde_json::to_value(PolicyDecisionOutcome::Deny).expect("json"),
            serde_json::json!("deny")
        );
    }

    #[test]
    fn ledger_kind_tags_are_snake_case() {
        let cases = [
            (
                LedgerKind::RunRecorded {
                    run_id: "r".into(),
                    run_root_hash: "rr".into(),
                    agent: "codex".into(),
                    task_hash: "th".into(),
                    event_count: 3,
                },
                "run_recorded",
            ),
            (
                LedgerKind::DiffRecorded {
                    run_id: "r".into(),
                    diff_hash: "dh".into(),
                    files: 1,
                    added: 2,
                    removed: 0,
                },
                "diff_recorded",
            ),
            (
                LedgerKind::PolicyDecision {
                    run_id: "r".into(),
                    policy_version: "v1".into(),
                    capability: "exec".into(),
                    decision: PolicyDecisionOutcome::Deny,
                    detail_hash: None,
                },
                "policy_decision",
            ),
            (
                LedgerKind::Verdict {
                    run_id: "r".into(),
                    diff_hash: None,
                    verdict: VerdictStatus::Passed,
                    checks: vec![],
                    advisory_llm_judge: None,
                    run_root_hash: None,
                },
                "verdict",
            ),
            (
                LedgerKind::ReceiptIssued {
                    run_id: "r".into(),
                    receipt_id: "rc".into(),
                    inputs_hash: "ih".into(),
                    policy_version: "v1".into(),
                    verdict: VerdictStatus::Passed,
                    run_root_hash: None,
                    verdict_id: None,
                },
                "receipt_issued",
            ),
            (
                LedgerKind::OutcomeRecorded {
                    run_id: "r".into(),
                    outcome: Outcome::Merged,
                    source: LabelSource::Human,
                    label_hash: "lh".into(),
                },
                "outcome_recorded",
            ),
        ];
        for (kind, tag) in cases {
            let value = serde_json::to_value(&kind).expect("kind json");
            assert_eq!(value["kind"], tag);
        }
    }

    #[test]
    fn outcome_recorded_round_trips_and_reuses_outcome_types() {
        let kind = LedgerKind::OutcomeRecorded {
            run_id: "run-9".into(),
            outcome: Outcome::Reverted,
            source: LabelSource::Ci,
            label_hash: "abc123".into(),
        };
        let value = serde_json::to_value(&kind).expect("outcome_recorded json");
        assert_eq!(value["kind"], "outcome_recorded");
        // The reused outcome/source enums keep their internally-tagged shape.
        assert_eq!(value["outcome"]["outcome"], "reverted");
        assert_eq!(value["source"]["source"], "ci");
        assert_eq!(value["label_hash"], "abc123");
        let back: LedgerKind = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, kind);
    }

    #[test]
    fn verdict_kind_reuses_verdict_types_and_round_trips() {
        let kind = LedgerKind::Verdict {
            run_id: "r1".into(),
            diff_hash: Some("dh".into()),
            verdict: VerdictStatus::Failed,
            checks: vec![sample_check()],
            advisory_llm_judge: Some(AdvisoryJudge {
                label: "looks-risky".into(),
                confidence_bp: 7500,
            }),
            run_root_hash: Some("run-root-abc".into()),
        };
        let value = serde_json::to_value(&kind).expect("verdict kind json");
        assert_eq!(value["kind"], "verdict");
        assert_eq!(value["verdict"], "failed");
        assert_eq!(value["checks"][0]["kind"], "test");
        assert_eq!(value["advisory_llm_judge"]["confidence_bp"], 7500);
        // The lineage edge (verdict→run) is carried verbatim.
        assert_eq!(value["run_root_hash"], "run-root-abc");
        let back: LedgerKind = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, kind);
    }

    #[test]
    fn receipt_issued_carries_lineage_edges_and_round_trips() {
        let kind = LedgerKind::ReceiptIssued {
            run_id: "r1".into(),
            receipt_id: "rc1".into(),
            inputs_hash: "ih".into(),
            policy_version: "v1".into(),
            verdict: VerdictStatus::Passed,
            run_root_hash: Some("run-root-xyz".into()),
            verdict_id: Some("verdict-content-id".into()),
        };
        let value = serde_json::to_value(&kind).expect("receipt_issued json");
        assert_eq!(value["kind"], "receipt_issued");
        // Both lineage edges (receipt→run, receipt→verdict) are carried verbatim.
        assert_eq!(value["run_root_hash"], "run-root-xyz");
        assert_eq!(value["verdict_id"], "verdict-content-id");
        let back: LedgerKind = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, kind);
    }

    /// ADDITIVE-INVARIANCE (serialization half — `trust-substrate.md` §3 L1, INV-R5):
    /// the new lineage edge fields are `Option` + `skip_serializing_if`, so a record
    /// built with them `None` serializes WITHOUT the new keys at all — byte-identical to
    /// a pre-change record. The companion hash-identity lock lives in the host's
    /// `ledger_store` tests, where the real `nerve_core::ledger` record-hash path is in
    /// scope (proto must not pull in the kernel).
    #[test]
    fn none_lineage_edges_omit_their_keys() {
        let verdict = LedgerKind::Verdict {
            run_id: "r".into(),
            diff_hash: None,
            verdict: VerdictStatus::Passed,
            checks: vec![],
            advisory_llm_judge: None,
            run_root_hash: None,
        };
        let v_value = serde_json::to_value(&verdict).expect("verdict json");
        assert!(v_value.get("run_root_hash").is_none());
        let v_back: LedgerKind = serde_json::from_value(v_value).expect("round-trip");
        assert_eq!(v_back, verdict);

        let receipt = LedgerKind::ReceiptIssued {
            run_id: "r".into(),
            receipt_id: "rc".into(),
            inputs_hash: "ih".into(),
            policy_version: "v1".into(),
            verdict: VerdictStatus::Passed,
            run_root_hash: None,
            verdict_id: None,
        };
        let r_value = serde_json::to_value(&receipt).expect("receipt json");
        assert!(r_value.get("run_root_hash").is_none());
        assert!(r_value.get("verdict_id").is_none());
        let r_back: LedgerKind = serde_json::from_value(r_value).expect("round-trip");
        assert_eq!(r_back, receipt);
    }

    #[test]
    fn policy_decision_omits_optional_detail_hash() {
        let kind = LedgerKind::PolicyDecision {
            run_id: "r".into(),
            policy_version: "v1".into(),
            capability: "egress".into(),
            decision: PolicyDecisionOutcome::Allow,
            detail_hash: None,
        };
        let value = serde_json::to_value(&kind).expect("decision json");
        assert!(value.get("detail_hash").is_none());
        let back: LedgerKind = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, kind);
    }

    #[test]
    fn ledger_record_round_trips_and_defaults_are_tolerant() {
        let record = LedgerRecord {
            schema_version: LEDGER_SCHEMA_VERSION,
            seq: 4,
            kind: LedgerKind::RunRecorded {
                run_id: "abc".into(),
                run_root_hash: "rr".into(),
                agent: "claude".into(),
                task_hash: "th".into(),
                event_count: 7,
            },
            record_hash: "ff".into(),
            prev_hash: "ee".into(),
            appended_at_ms: 1234,
        };
        let value = serde_json::to_value(&record).expect("record json");
        let back: LedgerRecord = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, record);

        // A minimal record (only the non-default fields) deserializes, with the
        // additive `appended_at_ms` falling back to its default — forward tolerance.
        let minimal: LedgerRecord = serde_json::from_value(serde_json::json!({
            "schema_version": LEDGER_SCHEMA_VERSION,
            "seq": 0,
            "kind": {"kind": "diff_recorded", "run_id": "r", "diff_hash": "dh",
                     "files": 1, "added": 0, "removed": 0},
            "record_hash": "aa",
            "prev_hash": "",
        }))
        .expect("minimal record");
        assert_eq!(minimal.appended_at_ms, 0);
    }

    #[test]
    fn ledger_head_default_and_round_trip() {
        let head = LedgerHead::default();
        assert_eq!(head.count, 0);
        assert_eq!(head.head_hash, "");

        let stamped = LedgerHead {
            schema_version: LEDGER_SCHEMA_VERSION,
            count: 3,
            head_hash: "cc".into(),
        };
        let value = serde_json::to_value(&stamped).expect("head json");
        let back: LedgerHead = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, stamped);

        // The additive `head_hash` is tolerant of omission (empty-log head).
        let minimal: LedgerHead = serde_json::from_value(serde_json::json!({
            "schema_version": LEDGER_SCHEMA_VERSION,
            "count": 0,
        }))
        .expect("minimal head");
        assert_eq!(minimal.head_hash, "");
    }
}
