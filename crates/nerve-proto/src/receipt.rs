//! Portable, signed **Verification Receipt** vocabulary — the **L4** trust unit
//! (`docs/designs/trust-substrate.md` §8). A receipt is the org-portable,
//! cryptographically signed attestation that a captured [`crate::provenance::Run`]
//! was replayed and cleared the org's own bar: it binds the run's provenance, the
//! re-verified checks, an aggregate verdict, and a replay manifest into a
//! [`ReceiptStatement`], then wraps that statement in a DSSE-style
//! [`ReceiptSignature`]. **Court reporter, not judge** (INV-R1): a receipt proves
//! *what an agent did, that it is replayable, and that it cleared the org's tests* —
//! never that the code is "correct".
//!
//! These are **pure, transport-neutral serde data** (INV-R5: receipts are portable,
//! signed, append-only, additive protocol data) with **no behavior** — every hash
//! and signature field is a plain `String`. The pure canonicalization + SHA-256
//! content-addressing + DSSE PAE encoding that *fills* those fields lives in
//! `nerve-core::receipt` (INV-R2: the hashing is pure and golden-tested), never
//! here. This crate only names the shapes so they are wasm-shareable and appear in
//! the exported protocol schema.
//!
//! **No floats** appear in any hashed type: timestamps are `u64` ms and are
//! host-supplied params excluded from the content address, so the canonical JSON is
//! byte-stable and the types derive `Eq` — exact golden snapshots, no precision or
//! `-0.0`/NaN nondeterminism (INV-R2).
//!
//! **[FIX-C]** The check kind and the verdict status are **reused** from
//! [`crate::verdict`] ([`crate::verdict::CheckKind`] / [`crate::verdict::VerdictStatus`]);
//! the receipt deliberately does NOT define its own `Verdict`/`CheckKind` enums.

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::policy::{EvidenceRequirement, MergeBar};
use crate::provenance::{IsolationTier, is_contained};
use crate::verdict::{CheckKind, VerdictStatus};

/// On-disk + on-wire receipt schema version. Bumped only for additive,
/// backward-compatible changes to the [`Receipt`] shape; a reader rejects a record
/// from a newer major it cannot understand rather than silently dropping fields.
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

/// The in-toto-style predicate type identifying a Nerve Verification Receipt
/// statement. Stamped into [`ReceiptStatement::predicate_type`] so a verifier can
/// recognize the attestation kind without parsing the payload.
pub const RECEIPT_PREDICATE_TYPE: &str = "https://nerve.dev/attestations/verification-receipt/v1";

/// One re-verified check summarized into the receipt: the org check `name`, its
/// [`CheckKind`], the per-check [`VerdictStatus`], whether the re-run was
/// `reproducible`, and an optional content hash of the captured evidence (logs /
/// artifacts). Reuses [`crate::verdict`]'s kind + status vocabulary verbatim
/// (**[FIX-C]**) — the receipt is a portable projection of the verdict, not a fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ReceiptCheck {
    /// The org check's stable name (matches the verdict's check name).
    pub name: String,
    /// What kind of check this is — reused from [`crate::verdict::CheckKind`].
    pub kind: CheckKind,
    /// Per-check verdict — reused from [`crate::verdict::VerdictStatus`].
    pub verdict: VerdictStatus,
    /// Whether re-running the check reproduced its recorded outcome.
    pub reproducible: bool,
    /// Optional SHA-256 of the captured evidence (logs / artifacts) for this check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_hash: Option<String>,
}

/// A pointer into the cross-run transparency [`crate::ledger`] log committing the
/// run: the `log_id`, the entry's `seq`, the entry's own `entry_hash`, and the
/// `prev_hash` it chains from. Lets a receipt holder independently locate and
/// re-verify the ledger inclusion of the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct LedgerRef {
    /// Identifier of the ledger log the run was appended to.
    pub log_id: String,
    /// The appended record's sequence number within the log.
    pub seq: u64,
    /// The appended record's own content hash.
    pub entry_hash: String,
    /// The hash the appended record chains from (the prior head).
    pub prev_hash: String,
}

/// The provenance binding of a receipt: which [`crate::provenance::Run`] it attests
/// (`run_id`), the content hash of that run's pinned inputs (`inputs_hash`), and
/// optional pins to the toolchain digest, the policy version in force, and the
/// transparency-[`LedgerRef`] committing the run. This is a *thinner* binding than
/// the L0c replay verdict — it commits provenance identity, not the full event tape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ReceiptProvenance {
    /// The attested run's identifier.
    pub run_id: String,
    /// Content hash of the run's pinned inputs (repo snapshot + toolchain).
    pub inputs_hash: String,
    /// Optional digest pinning the toolchain used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain_digest: Option<String>,
    /// Optional version of the policy doc in force when the run was gated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<String>,
    /// Optional pointer to the transparency-ledger entry committing the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_ref: Option<LedgerRef>,
    /// How strongly the L2 verify re-run that produced this receipt was contained — a
    /// probed FACT, never a request; downgrade-only (INV-R7). It lands INSIDE the signed
    /// statement (co-sealed, tamper-evident — INV-R5), so a verifier reading the receipt
    /// offline learns the verdict AND how hermetic the re-run was, and a
    /// `--require-isolation` gate floor can compare it. Omitted on the wire when
    /// [`IsolationTier::Contained`] (the default), so a receipt sealed before
    /// isolation-tier stamping is byte-identical and its `receipt_id` is unperturbed
    /// (additive-invariance).
    #[serde(default, skip_serializing_if = "is_contained")]
    pub isolation_tier: IsolationTier,
}

/// The receipt's replay binding: the run schema version replayed, the recorded
/// `root_hash` it reproduced, the `event_count`, and the optional replay `command`.
/// A thinner provenance binding than the canonical L0c [`crate::provenance::ReplayManifest`]
/// (different fields, different purpose) — kept distinct and re-exported as
/// `ReceiptReplayManifest` to avoid the name collision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ReplayManifest {
    /// The provenance schema version of the replayed run.
    pub run_schema_version: u32,
    /// The recorded content-address root hash the replay reproduced.
    pub root_hash: String,
    /// The number of events in the replayed tape.
    pub event_count: u64,
    /// Optional command line that drives the replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// The DSSE-style signature envelope over the canonical [`ReceiptStatement`] bytes:
/// the `payload_type`, the signing `backend` (e.g. `ed25519` / `sigstore`), the
/// `keyid`, the base64 `sig`, and optional `public_key` / Sigstore `bundle`. The
/// pure verifier in `nerve-core::receipt` re-derives the PAE and checks `sig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ReceiptSignature {
    /// The DSSE payload type the signature commits to.
    pub payload_type: String,
    /// The signing backend identifier (e.g. `ed25519`, `sigstore`).
    pub backend: String,
    /// The signing key identifier.
    pub keyid: String,
    /// The base64-encoded detached signature over the PAE-encoded statement.
    pub sig: String,
    /// Optional base64 public key for offline verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
    /// Optional Sigstore bundle (Fulcio cert + Rekor entry) when keyless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
}

/// The signed payload of a receipt — the in-toto-style statement. Binds the
/// [`ReceiptProvenance`], the re-verified [`ReceiptCheck`]s, the aggregate
/// [`VerdictStatus`] (empty checks ⇒ `Inconclusive`, **[FIX-C]**), the
/// [`ReplayManifest`], and the host-supplied `issued_at_ms`. Canonicalized and
/// hashed by `nerve-core::receipt` to produce the statement id and the signed bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ReceiptStatement {
    /// The predicate type identifying this attestation kind.
    pub predicate_type: String,
    /// The run-provenance binding the statement attests.
    pub provenance: ReceiptProvenance,
    /// The re-verified checks summarized into the statement.
    pub checks: Vec<ReceiptCheck>,
    /// The aggregate verdict over the checks — reused from [`crate::verdict`].
    pub verdict: VerdictStatus,
    /// The replay binding proving the run reproduces its recorded root hash.
    pub replay_manifest: ReplayManifest,
    /// Host wall-clock issuance time in ms (display metadata; not part of the id).
    pub issued_at_ms: u64,
    /// Content address of the checkspec the [`Self::checks`] were produced against —
    /// the receipt's copy of the sealed [`crate::verdict::Verdict::checkspec_hash`]
    /// (INV-R1). It binds the borrowed check *names* to a content-addressed checkspec
    /// identity so a [`crate::policy::MergeBar::expected_checkspec_hash`]-pinning bar can
    /// refuse a renamed/stubbed check impersonating a required one. Additive + omitted
    /// when absent (`None`), so a receipt sealed without a checkspec is byte-identical to
    /// a pre-binding receipt (additive-invariance).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkspec_hash: Option<String>,
    /// The org's sealed merge bar, **co-sealed into (and signed as part of) this
    /// statement** at issue time so the gate enforces the bar the receipt SIGNED —
    /// never a policy re-read from the gate host's disk (INV-R5: pin what is signed).
    /// Additive + omitted when empty (no `required_checks`), so a receipt sealed
    /// without an org bar is byte-identical to a pre-L3 receipt (additive-invariance).
    #[serde(default, skip_serializing_if = "MergeBar::is_empty")]
    pub merge_bar: MergeBar,
    /// The org's required-evidence predicates, co-sealed alongside [`Self::merge_bar`]
    /// (same signature, same portability). Additive + omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_evidence: Vec<EvidenceRequirement>,
}

/// A portable, signed Verification Receipt — the L4 unit of trust. Wraps the
/// content-addressed [`ReceiptStatement`] in a DSSE-style [`ReceiptSignature`].
/// `receipt_id` is the statement's content id; a holder can re-derive it and verify
/// the signature entirely offline with the embedded `public_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Receipt {
    /// The receipt schema version (see [`RECEIPT_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// The content id of [`Self::statement`] (also the receipt's identity).
    pub receipt_id: String,
    /// The signed statement payload.
    pub statement: ReceiptStatement,
    /// The DSSE-style signature over the canonical statement bytes.
    pub signature: ReceiptSignature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::{CheckKind, VerdictStatus};

    fn sample_statement() -> ReceiptStatement {
        ReceiptStatement {
            predicate_type: RECEIPT_PREDICATE_TYPE.to_string(),
            provenance: ReceiptProvenance {
                run_id: "run-1".into(),
                inputs_hash: "ih".into(),
                toolchain_digest: Some("tc".into()),
                policy_version: Some("pv".into()),
                ledger_ref: Some(LedgerRef {
                    log_id: "log".into(),
                    seq: 7,
                    entry_hash: "eh".into(),
                    prev_hash: "ph".into(),
                }),
                isolation_tier: IsolationTier::Contained,
            },
            checks: vec![ReceiptCheck {
                name: "cargo test".into(),
                kind: CheckKind::Test,
                verdict: VerdictStatus::Passed,
                reproducible: true,
                evidence_hash: Some("ev".into()),
            }],
            verdict: VerdictStatus::Passed,
            replay_manifest: ReplayManifest {
                run_schema_version: 2,
                root_hash: "rh".into(),
                event_count: 4,
                command: Some("nerve replay".into()),
            },
            issued_at_ms: 1234,
            checkspec_hash: None,
            merge_bar: MergeBar::default(),
            required_evidence: Vec::new(),
        }
    }

    fn sample_receipt() -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            receipt_id: "rid".into(),
            statement: sample_statement(),
            signature: ReceiptSignature {
                payload_type: "application/vnd.in-toto+json".into(),
                backend: "ed25519".into(),
                keyid: "k1".into(),
                sig: "c2ln".into(),
                public_key: Some("cGs=".into()),
                bundle: None,
            },
        }
    }

    #[test]
    fn receipt_round_trips() {
        let receipt = sample_receipt();
        let value = serde_json::to_value(&receipt).expect("receipt json");
        let back: Receipt = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, receipt);
    }

    #[test]
    fn receipt_check_reuses_verdict_vocabulary_tags() {
        // The kind/verdict serialize via verdict.rs's snake_case enums, confirming
        // the receipt reuses (not forks) the canonical vocabulary [FIX-C].
        let check = ReceiptCheck {
            name: "lint".into(),
            kind: CheckKind::Typecheck,
            verdict: VerdictStatus::Inconclusive,
            reproducible: false,
            evidence_hash: None,
        };
        let value = serde_json::to_value(&check).expect("check json");
        assert_eq!(value["kind"], "typecheck");
        assert_eq!(value["verdict"], "inconclusive");
        // Optional evidence_hash is omitted when None.
        assert!(value.get("evidence_hash").is_none());
        let back: ReceiptCheck = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, check);
    }

    #[test]
    fn optional_provenance_fields_are_omitted_when_none() {
        let prov = ReceiptProvenance {
            run_id: "r".into(),
            inputs_hash: "h".into(),
            toolchain_digest: None,
            policy_version: None,
            ledger_ref: None,
            isolation_tier: IsolationTier::Contained,
        };
        let value = serde_json::to_value(&prov).expect("prov json");
        assert!(value.get("toolchain_digest").is_none());
        assert!(value.get("policy_version").is_none());
        assert!(value.get("ledger_ref").is_none());
        // v15→v16 additive-invariance: the default Contained tier is omitted, so a
        // pre-isolation provenance is byte-identical and its receipt_id cannot churn.
        assert!(
            value.get("isolation_tier").is_none(),
            "default Contained isolation tier must be omitted"
        );
        let back: ReceiptProvenance = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, prov);
        // A pre-isolation provenance (no field) deserializes to the weak honest default.
        let legacy: ReceiptProvenance = serde_json::from_value(serde_json::json!({
            "run_id": "r", "inputs_hash": "h"
        }))
        .expect("legacy provenance");
        assert_eq!(legacy.isolation_tier, IsolationTier::Contained);
    }

    #[test]
    fn signature_omits_optional_public_key_and_bundle() {
        let sig = ReceiptSignature {
            payload_type: "pt".into(),
            backend: "ed25519".into(),
            keyid: "k".into(),
            sig: "s".into(),
            public_key: None,
            bundle: None,
        };
        let value = serde_json::to_value(&sig).expect("sig json");
        assert!(value.get("public_key").is_none());
        assert!(value.get("bundle").is_none());
        let back: ReceiptSignature = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, sig);
    }

    #[test]
    fn replay_manifest_omits_optional_command() {
        let manifest = ReplayManifest {
            run_schema_version: 2,
            root_hash: "rh".into(),
            event_count: 0,
            command: None,
        };
        let value = serde_json::to_value(&manifest).expect("manifest json");
        assert!(value.get("command").is_none());
        let back: ReplayManifest = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, manifest);
    }

    #[test]
    fn predicate_type_constant_is_stamped_into_statement() {
        let statement = sample_statement();
        let value = serde_json::to_value(&statement).expect("statement json");
        assert_eq!(value["predicate_type"], RECEIPT_PREDICATE_TYPE);
    }

    #[test]
    fn empty_merge_bar_and_evidence_are_omitted_for_additive_invariance() {
        // A statement with no org bar (the empty default) MUST omit both new keys so a
        // pre-L3 receipt's canonical bytes / content-id are byte-identical (v13→v14
        // additive-invariance — like every prior wave).
        let mut statement = sample_statement();
        statement.checkspec_hash = None;
        statement.merge_bar = MergeBar::default();
        statement.required_evidence = Vec::new();
        let value = serde_json::to_value(&statement).expect("statement json");
        assert!(value.get("merge_bar").is_none(), "empty bar key omitted");
        assert!(
            value.get("required_evidence").is_none(),
            "empty evidence key omitted"
        );
        assert!(
            value.get("checkspec_hash").is_none(),
            "absent checkspec key omitted (v14→v15 additive-invariance)"
        );
        // It still round-trips, with the additive fields falling back to their defaults.
        let back: ReceiptStatement = serde_json::from_value(value).expect("round-trip");
        assert!(back.merge_bar.is_empty());
        assert!(back.required_evidence.is_empty());
        assert!(back.checkspec_hash.is_none());
    }

    #[test]
    fn non_empty_merge_bar_and_evidence_serialize_and_round_trip() {
        let mut statement = sample_statement();
        statement.checkspec_hash = Some("spec-abc".into());
        statement.merge_bar = MergeBar {
            required_checks: vec!["unit".into(), "build".into()],
            expected_checkspec_hash: Some("spec-abc".into()),
        };
        statement.required_evidence = vec![EvidenceRequirement {
            kind: "receipt".into(),
        }];
        let value = serde_json::to_value(&statement).expect("statement json");
        assert_eq!(value["merge_bar"]["required_checks"][0], "unit");
        assert_eq!(value["merge_bar"]["expected_checkspec_hash"], "spec-abc");
        assert_eq!(value["checkspec_hash"], "spec-abc");
        assert_eq!(value["required_evidence"][0]["kind"], "receipt");
        let back: ReceiptStatement = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, statement);
    }
}
