//! Pure, golden-tested canonicalization + verification for the portable
//! **Verification Receipt** (L4 of `docs/designs/trust-substrate.md`, INV-R1). Given
//! a content-addressed [`Run`] (L0) and a set of per-check results, this module
//! aggregates the org's-own-test verdict, binds the run's provenance + replay
//! manifest into a [`ReceiptStatement`], canonicalizes that statement to stable
//! bytes, wraps it in a DSSE Pre-Authentication Encoding (PAE), derives a
//! content-addressed `statement_id`, and verifies an already-sealed [`Receipt`]
//! against an injected signature predicate.
//!
//! **Court reporter, not judge (INV-R1).** The verdict here is *borrowed* from the
//! org's own checks — an empty check set is honestly [`VerdictStatus::Inconclusive`]
//! (no bar exercised), never a fabricated pass.
//!
//! **Determinism boundary.** Everything here is a pure function of its arguments —
//! no IO, no wall-clock, no randomness, no signing (signing lives behind the
//! workstation `Signer` seam). `issued_at_ms` is host-supplied and carried in the
//! statement but timestamps never perturb the canonical bytes beyond their own
//! value. SHA-256 (via `sha2`) is the content-address primitive, mirroring
//! [`crate::provenance`].

// Re-export the receipt shapes this module operates on so a kernel consumer builds
// and verifies receipts through `nerve_core` alone, without its own `nerve-proto`
// dependency.
use nerve_proto::provenance::Run;
pub use nerve_proto::receipt::{
    LedgerRef, RECEIPT_PREDICATE_TYPE, RECEIPT_SCHEMA_VERSION, Receipt, ReceiptCheck,
    ReceiptProvenance, ReceiptSignature, ReceiptStatement, ReplayManifest,
};
use nerve_proto::verdict::VerdictStatus;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Outcome of [`verify_receipt`]: whether the statement still hashes to the sealed
/// `receipt_id` (integrity), whether the injected predicate accepted the signature
/// over the PAE bytes, and the verdict carried by the statement (echoed for the
/// caller's convenience).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptVerification {
    /// The statement re-hashes to the receipt's `receipt_id` (no tampering).
    pub statement_intact: bool,
    /// The injected signature predicate accepted the signature over the PAE bytes.
    pub signature_valid: bool,
    /// The verdict carried by the verified statement.
    pub verdict: VerdictStatus,
}

/// Aggregate the org's-own-test verdict from a set of receipt checks.
///
/// **INV-R1 honesty:** an empty check set is [`VerdictStatus::Inconclusive`] — no
/// bar was exercised, so no clearance is claimed (FIX-C). Any `Error` check makes
/// the whole verdict `Error`; otherwise any non-passing verdict makes it `Failed`;
/// otherwise (every check `Passed`) it is `Passed`.
#[must_use]
pub fn aggregate_verdict(checks: &[ReceiptCheck]) -> VerdictStatus {
    if checks.is_empty() {
        return VerdictStatus::Inconclusive;
    }
    if checks.iter().any(|c| c.verdict == VerdictStatus::Error) {
        return VerdictStatus::Error;
    }
    if checks.iter().any(|c| c.verdict != VerdictStatus::Passed) {
        return VerdictStatus::Failed;
    }
    VerdictStatus::Passed
}

/// Lowercase-hex SHA-256 of a run's content address — the receipt's `inputs_hash`.
/// Binds to [`Run::root_hash`] (the L0 spine over the whole event tape, which
/// already commits to agent/task/output), so the receipt's provenance is anchored
/// to the exact captured run. Deterministic and IO-free.
#[must_use]
pub fn hash_inputs(run: &Run) -> String {
    let mut hasher = Sha256::new();
    hasher.update(run.root_hash.as_bytes());
    hex(hasher.finalize().as_slice())
}

/// Build the receipt's thin replay-binding manifest from a sealed [`Run`]. This is
/// the *provenance* manifest (run schema version, root hash, event count) — distinct
/// from the L0c `provenance::ReplayManifest` (the replay verdict). `command` is left
/// `None` here; a host that knows the replay command supplies it separately.
#[must_use]
pub fn replay_manifest_for(run: &Run) -> ReplayManifest {
    ReplayManifest {
        run_schema_version: run.schema_version,
        root_hash: run.root_hash.clone(),
        event_count: run.events.len() as u64,
        command: None,
    }
}

/// Assemble the unsigned [`ReceiptStatement`] for a run: the in-toto-style predicate
/// type, the run's provenance (inputs hash + optional toolchain/policy/ledger refs),
/// the per-check results, the **borrowed** verdict, the replay manifest, and the
/// host-supplied issue time. Pure — `issued_at_ms` is a param, never `now()`.
///
/// **INV-R1 (court reporter):** `verdict` is the org's own sealed verdict (the L2
/// [`Verdict::status`](crate::verdict::Verdict), which honours the per-check `required`
/// mask), passed in and echoed verbatim — it is NOT re-derived from `checks`. The
/// `checks` are carried as *evidence* only. Re-aggregating here would let the receipt
/// assert a stricter-or-looser verdict than the org granted (e.g. an all-advisory
/// checkspec minting a PASS); borrowing forbids that. Callers that have no sealed
/// verdict can compute one with [`aggregate_verdict`] and pass it explicitly.
#[allow(clippy::too_many_arguments)] // reason: faithful 1:1 binding of the statement's fields
#[must_use]
pub fn build_statement(
    run: &Run,
    checks: Vec<ReceiptCheck>,
    verdict: VerdictStatus,
    toolchain_digest: Option<String>,
    policy_version: Option<String>,
    ledger_ref: Option<LedgerRef>,
    issued_at_ms: u64,
) -> ReceiptStatement {
    ReceiptStatement {
        predicate_type: RECEIPT_PREDICATE_TYPE.to_string(),
        provenance: ReceiptProvenance {
            run_id: run.run_id.clone(),
            inputs_hash: hash_inputs(run),
            toolchain_digest,
            policy_version,
            ledger_ref,
        },
        checks,
        verdict,
        replay_manifest: replay_manifest_for(run),
        issued_at_ms,
    }
}

/// Canonical bytes of a statement: its `serde_json` serialization. Every field is a
/// fixed-field struct, an internally-tagged enum, or an integer — **no maps, no
/// floats** — so `serde_json` emits byte-stable output (INV-R2). These are the bytes
/// the DSSE PAE wraps and the signature covers.
#[must_use]
pub fn canonical_statement_bytes(statement: &ReceiptStatement) -> Vec<u8> {
    serde_json::to_vec(statement).expect("ReceiptStatement serializes infallibly")
}

/// DSSE Pre-Authentication Encoding (in-toto/DSSE spec): the byte string
/// `"DSSEv1 " || len(type) || " " || type || " " || len(payload) || " " || payload`,
/// where lengths are ASCII-decimal byte counts. Signing the PAE (not the raw
/// payload) binds the signature to the payload *type*, preventing cross-type
/// confusion. Pure and deterministic.
#[must_use]
pub fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

/// Content-address a statement: lowercase-hex SHA-256 over its canonical bytes. This
/// is the receipt's `receipt_id` — a tamper-evident handle that re-deriving from the
/// stored statement must reproduce exactly (else [`verify_receipt`] reports a broken
/// `statement_intact`).
#[must_use]
pub fn statement_id(statement: &ReceiptStatement) -> String {
    hex(Sha256::digest(canonical_statement_bytes(statement)).as_slice())
}

/// Seal an already-signed statement into a [`Receipt`], stamping its content-address
/// as the `receipt_id`. NO signing happens here — the [`ReceiptSignature`] is produced
/// by the workstation `Signer` seam and passed in. Pure.
#[must_use]
pub fn seal_receipt(statement: ReceiptStatement, signature: ReceiptSignature) -> Receipt {
    Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        receipt_id: statement_id(&statement),
        statement,
        signature,
    }
}

/// Verify a sealed [`Receipt`] without owning any crypto: re-derive the statement's
/// content address (integrity), then ask the injected `verify_sig` predicate whether
/// the signature is valid over the DSSE PAE bytes. `verify_sig` is called as
/// `verify_sig(public_key, pae, signature)` — the host supplies the actual ed25519 /
/// sigstore verification (the kernel stays crypto-free). The signature's bytes and
/// public key are decoded by the host's predicate, so this fn passes them through
/// as raw UTF-8 of whatever encoding the signature carries.
#[must_use]
pub fn verify_receipt(
    receipt: &Receipt,
    verify_sig: impl Fn(&[u8], &[u8], &[u8]) -> bool,
) -> ReceiptVerification {
    let statement_intact = statement_id(&receipt.statement) == receipt.receipt_id;
    let payload = canonical_statement_bytes(&receipt.statement);
    let pae = dsse_pae(&receipt.signature.payload_type, &payload);
    let public_key = receipt
        .signature
        .public_key
        .as_deref()
        .unwrap_or_default()
        .as_bytes();
    let signature_valid = verify_sig(public_key, &pae, receipt.signature.sig.as_bytes());
    ReceiptVerification {
        statement_intact,
        signature_valid,
        verdict: receipt.statement.verdict,
    }
}

/// Lowercase-hex encode bytes (mirrors [`crate::provenance`]'s `hex`).
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_proto::provenance::{Event, EventKind};
    use nerve_proto::verdict::CheckKind;

    fn sample_run() -> Run {
        crate::provenance::build_run(
            "job-1",
            "codex",
            Some("/repo".into()),
            1000,
            Some(2000),
            true,
            vec![Event {
                seq: 0,
                kind: EventKind::RunStarted {
                    agent: "codex".into(),
                    task: "add a test".into(),
                    cwd: Some("/repo".into()),
                    inputs: None,
                },
            }],
            nerve_proto::provenance::RunInputs::default(),
        )
    }

    fn check(name: &str, verdict: VerdictStatus) -> ReceiptCheck {
        ReceiptCheck {
            name: name.into(),
            kind: CheckKind::Test,
            verdict,
            reproducible: true,
            evidence_hash: None,
        }
    }

    fn signature() -> ReceiptSignature {
        ReceiptSignature {
            payload_type: RECEIPT_PREDICATE_TYPE.into(),
            backend: "local-ed25519".into(),
            keyid: "test-key".into(),
            sig: "sig-bytes".into(),
            public_key: Some("pub-bytes".into()),
            bundle: None,
        }
    }

    #[test]
    fn aggregate_verdict_borrows_from_checks() {
        // Empty checks => Inconclusive (no bar exercised), never a fabricated pass.
        assert_eq!(aggregate_verdict(&[]), VerdictStatus::Inconclusive);
        // All passing => Passed.
        assert_eq!(
            aggregate_verdict(&[
                check("a", VerdictStatus::Passed),
                check("b", VerdictStatus::Passed),
            ]),
            VerdictStatus::Passed
        );
        // Any non-pass (non-error) => Failed.
        assert_eq!(
            aggregate_verdict(&[
                check("a", VerdictStatus::Passed),
                check("b", VerdictStatus::Failed),
            ]),
            VerdictStatus::Failed
        );
        // Inconclusive among passes still drops the whole verdict below Passed.
        assert_eq!(
            aggregate_verdict(&[
                check("a", VerdictStatus::Passed),
                check("b", VerdictStatus::Inconclusive),
            ]),
            VerdictStatus::Failed
        );
        // Any Error dominates.
        assert_eq!(
            aggregate_verdict(&[
                check("a", VerdictStatus::Failed),
                check("b", VerdictStatus::Error),
            ]),
            VerdictStatus::Error
        );
    }

    #[test]
    fn hash_inputs_binds_to_run_root_and_is_deterministic() {
        let run = sample_run();
        let h1 = hash_inputs(&run);
        let h2 = hash_inputs(&run);
        assert_eq!(h1, h2, "same run -> same inputs hash");
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn replay_manifest_mirrors_run() {
        let run = sample_run();
        let manifest = replay_manifest_for(&run);
        assert_eq!(manifest.root_hash, run.root_hash);
        assert_eq!(manifest.run_schema_version, run.schema_version);
        assert_eq!(manifest.event_count, run.events.len() as u64);
        assert_eq!(manifest.command, None);
    }

    #[test]
    fn build_statement_borrows_the_verdict_and_binds_provenance() {
        let run = sample_run();
        // The verdict is BORROWED, not re-derived from `checks`: pass Passed even though
        // a carried check Failed (it was advisory at L2) — the receipt echoes Passed.
        // Re-aggregating would have fabricated a Failed the org never granted (INV-R1).
        let stmt = build_statement(
            &run,
            vec![
                check("required", VerdictStatus::Passed),
                check("advisory", VerdictStatus::Failed),
            ],
            VerdictStatus::Passed,
            Some("toolchain-x".into()),
            Some("policy-1".into()),
            None,
            5000,
        );
        assert_eq!(stmt.predicate_type, RECEIPT_PREDICATE_TYPE);
        assert_eq!(
            stmt.verdict,
            VerdictStatus::Passed,
            "verdict is borrowed verbatim, not re-aggregated from the evidence checks"
        );
        assert_eq!(stmt.provenance.run_id, run.run_id);
        assert_eq!(stmt.provenance.inputs_hash, hash_inputs(&run));
        assert_eq!(stmt.issued_at_ms, 5000);
        // An Inconclusive org verdict (e.g. all-advisory checkspec) is carried through
        // verbatim — never silently upgraded to a fabricated pass.
        let inconclusive = build_statement(
            &run,
            vec![check("advisory", VerdictStatus::Passed)],
            VerdictStatus::Inconclusive,
            None,
            None,
            None,
            5000,
        );
        assert_eq!(inconclusive.verdict, VerdictStatus::Inconclusive);
    }

    #[test]
    fn dsse_pae_encodes_type_and_length() {
        let pae = dsse_pae("application/x", b"hello");
        assert_eq!(pae, b"DSSEv1 13 application/x 5 hello".to_vec());
        // Distinct payload types yield distinct PAE bytes (cross-type confusion guard).
        let other = dsse_pae("application/y", b"hello");
        assert_ne!(pae, other);
    }

    #[test]
    fn statement_id_is_stable_and_content_addressed() {
        let run = sample_run();
        let stmt = build_statement(
            &run,
            vec![check("t", VerdictStatus::Passed)],
            VerdictStatus::Passed,
            None,
            None,
            None,
            1,
        );
        let id1 = statement_id(&stmt);
        let id2 = statement_id(&stmt);
        assert_eq!(id1, id2, "same statement -> same id");
        assert_eq!(id1.len(), 64);
        // A changed field changes the content address.
        let mut tampered = stmt.clone();
        tampered.issued_at_ms = 999;
        assert_ne!(id1, statement_id(&tampered));
    }

    #[test]
    fn seal_stamps_content_address() {
        let run = sample_run();
        let stmt = build_statement(
            &run,
            vec![check("t", VerdictStatus::Passed)],
            VerdictStatus::Passed,
            None,
            None,
            None,
            1,
        );
        let expected = statement_id(&stmt);
        let receipt = seal_receipt(stmt, signature());
        assert_eq!(receipt.receipt_id, expected);
        assert_eq!(receipt.schema_version, RECEIPT_SCHEMA_VERSION);
    }

    #[test]
    fn verify_receipt_detects_integrity_and_delegates_signature() {
        let run = sample_run();
        let stmt = build_statement(
            &run,
            vec![check("t", VerdictStatus::Passed)],
            VerdictStatus::Passed,
            None,
            None,
            None,
            1,
        );
        let receipt = seal_receipt(stmt, signature());

        // Intact receipt + accepting predicate => both flags true; verdict echoed.
        let ok = verify_receipt(&receipt, |_pk, _pae, _sig| true);
        assert!(ok.statement_intact);
        assert!(ok.signature_valid);
        assert_eq!(ok.verdict, VerdictStatus::Passed);

        // The predicate receives the PAE over the canonical statement bytes.
        let expected_pae = dsse_pae(
            &receipt.signature.payload_type,
            &canonical_statement_bytes(&receipt.statement),
        );
        let observed = verify_receipt(&receipt, |pk, pae, sig| {
            pk == b"pub-bytes" && pae == expected_pae.as_slice() && sig == b"sig-bytes"
        });
        assert!(observed.signature_valid);

        // Rejecting predicate => signature_valid false, integrity still intact.
        let bad_sig = verify_receipt(&receipt, |_pk, _pae, _sig| false);
        assert!(bad_sig.statement_intact);
        assert!(!bad_sig.signature_valid);

        // Tampering the statement breaks integrity while leaving the stale id.
        let mut tampered = receipt.clone();
        tampered.statement.issued_at_ms = 42;
        let detected = verify_receipt(&tampered, |_pk, _pae, _sig| true);
        assert!(!detected.statement_intact);
    }
}
