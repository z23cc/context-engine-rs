//! Pure, golden-tested **merge-gate decision** for a signed Verification Receipt
//! (`docs/designs/trust-substrate.md` §8 L5, INV-R1). Given a [`Receipt`] this maps
//! its aggregate [`VerdictStatus`] to the tri-state outcome a CI/merge surface
//! consumes: a process **exit code**, a stable **conclusion** label (the vocabulary
//! GitHub Checks / GitLab commit-status expect), and a one-line human **summary**.
//!
//! **Court reporter, not judge (INV-R1).** The gate does not decide whether the code
//! is "correct" — it only borrows the receipt's already-sealed verdict (which itself
//! borrows the org's own tests) and translates it into a transport-neutral decision.
//! `Inconclusive`/`Error` are treated as **neutral** (no bar exercised / harness
//! failure) and surface exit code 2, so an un-cleared change never reads as a pass.
//!
//! This is the determinism boundary's L5 brick: a pure function of the receipt —
//! no IO, no wall-clock, no randomness — so the same receipt yields a byte-identical
//! [`GateOutcome`]. Side-effecting emission (posting a GitHub check run, exiting the
//! process) lives above the kernel in `nerve-workstation` (INV-R2).

// Re-export the verdict vocabulary this gate reads so a kernel consumer can match on
// the same `VerdictStatus` it sees in a `GateOutcome`'s upstream without taking its
// own `nerve-proto` dependency.
pub use nerve_proto::receipt::Receipt;
pub use nerve_proto::verdict::VerdictStatus;

use nerve_proto::policy::{EvidenceRequirement, MergeBar};
use nerve_proto::provenance::IsolationTier;
use serde::{Deserialize, Serialize};

/// The tri-state merge-gate decision derived from a receipt's aggregate verdict.
///
/// `exit_code` is authoritative for a CI step (0 pass / 1 fail / 2 neutral);
/// `conclusion` is the stable label a checks API expects (`"success"` / `"failure"`
/// / `"neutral"` / `"error"`); `summary` is a short human-readable rationale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateOutcome {
    /// Process exit code: `0` on cleared, `1` on a cleared-but-failed bar, `2` when
    /// the bar was not exercised (inconclusive) or the harness errored.
    pub exit_code: i32,
    /// Stable conclusion label for a checks API: `success` / `failure` / `neutral`
    /// / `error`.
    pub conclusion: String,
    /// One-line human-readable rationale for the decision.
    pub summary: String,
}

/// Decide the merge-gate outcome for a signed receipt by borrowing its sealed
/// aggregate verdict (INV-R1). Pure: the same receipt always yields the same
/// [`GateOutcome`].
///
/// Mapping (trust-substrate RISK §6): `Passed → (0, "success")`,
/// `Failed → (1, "failure")`, `Inconclusive → (2, "neutral")`, `Error → (2, "error")`.
#[must_use]
pub fn gate_outcome(receipt: &Receipt) -> GateOutcome {
    match receipt.statement.verdict {
        VerdictStatus::Passed => GateOutcome {
            exit_code: 0,
            conclusion: "success".to_string(),
            summary: "verdict passed: the org's required checks cleared on replay".to_string(),
        },
        VerdictStatus::Failed => GateOutcome {
            exit_code: 1,
            conclusion: "failure".to_string(),
            summary: "verdict failed: a required check did not clear the org's bar".to_string(),
        },
        VerdictStatus::Inconclusive => GateOutcome {
            exit_code: 2,
            conclusion: "neutral".to_string(),
            summary: "verdict inconclusive: no required bar was exercised".to_string(),
        },
        VerdictStatus::Error => GateOutcome {
            exit_code: 2,
            conclusion: "error".to_string(),
            summary: "verdict error: the verification harness failed to run".to_string(),
        },
    }
}

/// The result of folding the org's co-sealed merge bar over a receipt's resident data
/// (L3, INV-R1). Records exactly which required checks were missing or unmet and which
/// required evidence was absent, plus whether the bar was actually *exercised* — so the
/// human summary enumerates the org's bar clearance, never a fabricated rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BarReport {
    /// Required check names that did not appear at all in the receipt's checks.
    missing_checks: Vec<String>,
    /// Required check names that appeared but did not pass (with their status).
    failed_checks: Vec<String>,
    /// Required evidence kinds that were absent or of an unknown (fail-closed) kind.
    unmet_evidence: Vec<String>,
}

impl BarReport {
    /// The bar is fully cleared iff nothing was missing, failed, or unmet.
    fn is_clear(&self) -> bool {
        self.missing_checks.is_empty()
            && self.failed_checks.is_empty()
            && self.unmet_evidence.is_empty()
    }

    /// A required check appeared but did not pass (the bar was *exercised* and not met)
    /// — distinct from an absent check (the bar was not exercised). Drives the
    /// downgrade direction: a present-and-failed required check is exit 1 (failure);
    /// anything merely missing/incomplete is exit 2 (neutral) — never a fabricated pass.
    fn has_exercised_failure(&self) -> bool {
        !self.failed_checks.is_empty()
    }

    /// One-line human enumeration of exactly what the bar found, appended to the gate
    /// summary so a consumer sees the org's bar clearance (or its gaps).
    fn summary(&self) -> String {
        if self.is_clear() {
            return "merge bar cleared: all required checks passed and required evidence present"
                .to_string();
        }
        let mut parts = Vec::new();
        for name in &self.missing_checks {
            parts.push(format!("missing required check '{name}'"));
        }
        for entry in &self.failed_checks {
            parts.push(format!("required check {entry}"));
        }
        for kind in &self.unmet_evidence {
            parts.push(format!("required evidence '{kind}' absent"));
        }
        format!("merge bar not cleared: {}", parts.join("; "))
    }
}

/// Enforce the org's **co-sealed** merge bar (L3) over a signed receipt, returning the
/// bar-aware merge-gate decision (`docs/designs/frontier-l3-l6-sigstore.md` §1, INV-R1).
///
/// **Pin what is signed (INV-R5).** The bar is read from the receipt's embedded
/// `statement.merge_bar` and `statement.required_evidence` — co-sealed (and signed as
/// part of) the statement at seal time — never from the gate host's live policy plane.
/// Because it is in the signed statement, the wave-7 `verify_receipt` refusal already
/// protects it from gate-side tampering.
///
/// **Downgrade-only, court reporter (INV-R1).**
///
/// - If the base [`gate_outcome`] is already non-success (exit != 0), it is returned
///   **unchanged except** the bar report is APPENDED to its summary — never replaced,
///   never upgraded.
/// - An empty bar (no `required_checks`) and no `required_evidence` is a pure
///   pass-through (today's behavior; no regression).
/// - A success base with a required check **present-and-failed** → exit 1 (failure: the
///   bar was exercised and not met).
/// - A success base with a required check **missing** or any required evidence **absent
///   / of an unknown kind** → exit 2 (neutral: the bar was not exercised / incomplete —
///   never a fabricated pass).
///
/// **🔒 Checkspec-identity binding (INV-R1 — `frontier-l3-l6-sigstore.md` §1 fix #2).**
/// Before any name-matching, if the bar pins an `expected_checkspec_hash` the receipt's
/// own `statement.checkspec_hash` MUST equal it — otherwise a renamed or stubbed
/// (`command:'true'`) check could impersonate the org's real check by reusing its display
/// name. A mismatch (or an absent receipt checkspec) means the required-check names cannot
/// be trusted, so the bar is treated as NOT exercised and the gate downgrades to neutral
/// (a non-success base is kept, never upgraded). A bar that pins no expected hash keeps the
/// pre-binding by-name behavior unchanged (backward compatible with v14).
///
/// Pure: no `Path`, no `fs`, no clock — a function of the receipt alone (INV-R2).
#[must_use]
pub fn enforce_merge_bar(receipt: &Receipt) -> GateOutcome {
    let base = gate_outcome(receipt);
    let bar = &receipt.statement.merge_bar;
    let evidence = &receipt.statement.required_evidence;

    // 🔒 Checkspec-identity gate (downgrade-only, INV-R1). The required-check NAMES are
    // only trustworthy if the receipt was verified against the exact checkspec the bar was
    // authored against; a mismatch (or no pinned receipt checkspec) downgrades to neutral.
    if let Some(expected) = &bar.expected_checkspec_hash {
        let actual = receipt.statement.checkspec_hash.as_deref();
        if actual != Some(expected.as_str()) {
            return checkspec_mismatch_outcome(base, expected, actual);
        }
    }

    // Empty bar => pure pass-through (today's behavior, no regression).
    if bar.required_checks.is_empty() && evidence.is_empty() {
        return base;
    }

    let report = build_bar_report(receipt, bar, evidence);

    // A non-success base verdict is NEVER upgraded: keep it, append the bar report so the
    // org's bar clearance is still surfaced (INV-R1 — downgrade-only).
    if base.exit_code != 0 {
        return GateOutcome {
            summary: format!("{} [{}]", base.summary, report.summary()),
            ..base
        };
    }

    // Base is success. The bar may only KEEP the pass (cleared) or DOWNGRADE it.
    if report.is_clear() {
        return GateOutcome {
            summary: format!("{} [{}]", base.summary, report.summary()),
            ..base
        };
    }
    let (exit_code, conclusion) = if report.has_exercised_failure() {
        // A required check present-and-failed: the bar was exercised and not met.
        (1, "failure")
    } else {
        // A required check missing / evidence absent: the bar was not exercised /
        // incomplete — neutral, never a fabricated pass.
        (2, "neutral")
    };
    GateOutcome {
        exit_code,
        conclusion: conclusion.to_string(),
        summary: report.summary(),
    }
}

/// Enforce an OPTIONAL **isolation-tier floor** over an already-decided gate outcome
/// (INV-R7, `docs/designs/hermetic-replay-isolation.md` §3.4) — the org's
/// `nerve gate --require-isolation <tier>` lever. Applied AFTER the wave-7 signature
/// verify and the merge-bar enforcement, reusing the same **downgrade-only** kernel
/// (INV-R1): if the receipt's signed `provenance.isolation_tier` is BELOW `required`,
/// a passing outcome is downgraded to **neutral** (exit 2) with the shortfall as its
/// summary — NEVER upgraded, never a fabricated pass; a non-success base is kept verbatim
/// with the shortfall appended. `None` (no floor requested) and a met floor are pure
/// pass-throughs (default report-only behavior, unchanged).
///
/// Pure: a function of the receipt + the required tier alone — no IO, no clock (INV-R2).
#[must_use]
pub fn enforce_isolation_floor(
    base: GateOutcome,
    receipt: &Receipt,
    required: Option<IsolationTier>,
) -> GateOutcome {
    let Some(required) = required else {
        return base; // no floor requested — report-only, unchanged.
    };
    let actual = receipt.statement.provenance.isolation_tier;
    if actual >= required {
        return base; // floor met — the tier is at or above the required strength.
    }
    let note = format!(
        "isolation tier {} below required {}",
        isolation_label(actual),
        isolation_label(required),
    );
    // Downgrade-only (INV-R1/R7): NEVER upgrade a non-success base — keep it and append
    // the shortfall so the floor's finding is still surfaced.
    if base.exit_code != 0 {
        return GateOutcome {
            summary: format!("{} [{}]", base.summary, note),
            ..base
        };
    }
    // A success base under an unmet floor becomes neutral — a non-bit-for-bit re-run
    // never clears a higher-tier requirement (never a fabricated pass).
    GateOutcome {
        exit_code: 2,
        conclusion: "neutral".to_string(),
        summary: note,
    }
}

/// Lowercase, hyphenated label for an [`IsolationTier`] (for the human floor summary),
/// matching the `--require-isolation` flag spelling.
fn isolation_label(tier: IsolationTier) -> &'static str {
    match tier {
        IsolationTier::Hermetic => "hermetic",
        IsolationTier::Contained => "contained",
        IsolationTier::BestEffort => "best-effort",
        IsolationTier::Unconfined => "unconfined",
    }
}

/// Downgrade-only outcome for a checkspec-identity MISMATCH (INV-R1). The bar pinned a
/// checkspec the receipt was **not** verified against, so the required-check names cannot
/// be trusted. A success base is downgraded to neutral (exit 2); a non-success base is
/// kept verbatim with the mismatch surfaced in its summary — never an upgrade, never a
/// fabricated pass.
fn checkspec_mismatch_outcome(
    base: GateOutcome,
    expected: &str,
    actual: Option<&str>,
) -> GateOutcome {
    let note = format!(
        "merge bar authored against checkspec {} but receipt was verified against {} — \
         required checks cannot be trusted",
        short_hash(expected),
        actual.map_or_else(|| "none".to_string(), short_hash),
    );
    if base.exit_code != 0 {
        // Never upgrade a non-success base: keep it, append the mismatch (downgrade-only).
        return GateOutcome {
            summary: format!("{} [{}]", base.summary, note),
            ..base
        };
    }
    GateOutcome {
        exit_code: 2,
        conclusion: "neutral".to_string(),
        summary: note,
    }
}

/// First 8 chars of a content address (or the whole string if shorter) for a compact,
/// human-readable mismatch summary. Char-boundary-safe (hex digests are ASCII).
fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Fold the co-sealed bar over the receipt's resident checks + provenance into a
/// [`BarReport`] (the pure heart of [`enforce_merge_bar`]).
fn build_bar_report(
    receipt: &Receipt,
    bar: &MergeBar,
    evidence: &[EvidenceRequirement],
) -> BarReport {
    let mut missing_checks = Vec::new();
    let mut failed_checks = Vec::new();
    for required in &bar.required_checks {
        match receipt
            .statement
            .checks
            .iter()
            .find(|c| &c.name == required)
        {
            None => missing_checks.push(required.clone()),
            Some(check) if check.verdict != VerdictStatus::Passed => {
                failed_checks.push(format!(
                    "'{}' did not pass ({})",
                    required,
                    verdict_label(check.verdict)
                ));
            }
            Some(_) => {}
        }
    }
    let mut unmet_evidence = Vec::new();
    for requirement in evidence {
        if !evidence_satisfied(receipt, &requirement.kind) {
            unmet_evidence.push(requirement.kind.clone());
        }
    }
    BarReport {
        missing_checks,
        failed_checks,
        unmet_evidence,
    }
}

/// Whether a required evidence `kind` is satisfied by the receipt's resident provenance.
/// **Closed, fail-closed enum (INV-R3):** an unknown kind is UNSATISFIED. No threshold /
/// coverage / "diff touches tested files" predicates (those drift into a judge).
fn evidence_satisfied(receipt: &Receipt, kind: &str) -> bool {
    let provenance = &receipt.statement.provenance;
    match kind {
        // The receipt itself exists (it is the thing we are gating).
        "receipt" => true,
        // A non-empty replay manifest root hash proves the run replays.
        "replay" => !receipt.statement.replay_manifest.root_hash.is_empty(),
        // A transparency-ledger pointer commits the run.
        "ledger" => provenance.ledger_ref.is_some(),
        // The receipt pinned the in-force policy version.
        "policy" => provenance.policy_version.is_some(),
        // Fail-closed: an unrecognized evidence kind is never satisfied.
        _ => false,
    }
}

/// Lowercase display label for a per-check verdict (for the human bar summary).
fn verdict_label(status: VerdictStatus) -> &'static str {
    match status {
        VerdictStatus::Passed => "passed",
        VerdictStatus::Failed => "failed",
        VerdictStatus::Inconclusive => "inconclusive",
        VerdictStatus::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_proto::receipt::{
        RECEIPT_PREDICATE_TYPE, RECEIPT_SCHEMA_VERSION, Receipt, ReceiptProvenance,
        ReceiptSignature, ReceiptStatement, ReplayManifest,
    };

    fn receipt_with(verdict: VerdictStatus) -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            receipt_id: "rcpt-deadbeef".to_string(),
            statement: ReceiptStatement {
                predicate_type: RECEIPT_PREDICATE_TYPE.to_string(),
                provenance: ReceiptProvenance {
                    run_id: "run-1".to_string(),
                    inputs_hash: "abc".to_string(),
                    toolchain_digest: None,
                    policy_version: None,
                    ledger_ref: None,
                    isolation_tier: IsolationTier::Contained,
                },
                checks: vec![],
                verdict,
                replay_manifest: ReplayManifest {
                    run_schema_version: 2,
                    root_hash: "root".to_string(),
                    event_count: 0,
                    command: None,
                },
                issued_at_ms: 1000,
                checkspec_hash: None,
                merge_bar: MergeBar::default(),
                required_evidence: Vec::new(),
            },
            signature: ReceiptSignature {
                payload_type: "application/vnd.in-toto+json".to_string(),
                backend: "local-ed25519".to_string(),
                keyid: "k1".to_string(),
                sig: "sig".to_string(),
                public_key: None,
                bundle: None,
            },
        }
    }

    #[test]
    fn passed_maps_to_success_exit_zero() {
        let out = gate_outcome(&receipt_with(VerdictStatus::Passed));
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.conclusion, "success");
        assert!(!out.summary.is_empty());
    }

    #[test]
    fn failed_maps_to_failure_exit_one() {
        let out = gate_outcome(&receipt_with(VerdictStatus::Failed));
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.conclusion, "failure");
    }

    #[test]
    fn inconclusive_maps_to_neutral_exit_two() {
        let out = gate_outcome(&receipt_with(VerdictStatus::Inconclusive));
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
    }

    #[test]
    fn error_maps_to_error_exit_two() {
        let out = gate_outcome(&receipt_with(VerdictStatus::Error));
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "error");
    }

    #[test]
    fn outcome_is_deterministic_for_same_receipt() {
        let receipt = receipt_with(VerdictStatus::Passed);
        assert_eq!(gate_outcome(&receipt), gate_outcome(&receipt));
    }

    #[test]
    fn distinct_verdicts_yield_distinct_outcomes() {
        let passed = gate_outcome(&receipt_with(VerdictStatus::Passed));
        let failed = gate_outcome(&receipt_with(VerdictStatus::Failed));
        let inconclusive = gate_outcome(&receipt_with(VerdictStatus::Inconclusive));
        let error = gate_outcome(&receipt_with(VerdictStatus::Error));
        // Every conclusion label is distinct.
        assert_ne!(passed.conclusion, failed.conclusion);
        assert_ne!(inconclusive.conclusion, error.conclusion);
        // Only a clean pass exits 0; an un-cleared change never reads as success.
        assert_eq!(passed.exit_code, 0);
        assert_ne!(failed.exit_code, 0);
        assert_ne!(inconclusive.exit_code, 0);
        assert_ne!(error.exit_code, 0);
        // Neutral and error share exit 2 but differ in conclusion (RISK §6).
        assert_eq!(inconclusive.exit_code, error.exit_code);
    }

    // --- L3 merge-bar enforcement overlay (enforce_merge_bar) ---

    use nerve_proto::receipt::ReceiptCheck;
    use nerve_proto::verdict::CheckKind;

    /// A receipt with the given aggregate verdict, an explicit set of per-check results,
    /// the co-sealed merge bar, and the required-evidence list — the dial board for the
    /// enforcement matrix.
    fn receipt_with_bar(
        verdict: VerdictStatus,
        checks: Vec<(&str, VerdictStatus)>,
        required_checks: Vec<&str>,
        required_evidence: Vec<&str>,
    ) -> Receipt {
        let mut receipt = receipt_with(verdict);
        receipt.statement.checks = checks
            .into_iter()
            .map(|(name, v)| ReceiptCheck {
                name: name.to_string(),
                kind: CheckKind::Test,
                verdict: v,
                reproducible: true,
                evidence_hash: None,
            })
            .collect();
        receipt.statement.merge_bar = MergeBar {
            required_checks: required_checks.into_iter().map(str::to_owned).collect(),
            expected_checkspec_hash: None,
        };
        receipt.statement.required_evidence = required_evidence
            .into_iter()
            .map(|kind| EvidenceRequirement {
                kind: kind.to_string(),
            })
            .collect();
        receipt
    }

    #[test]
    fn empty_bar_is_pure_passthrough() {
        // No required checks + no required evidence => identical to gate_outcome.
        for verdict in [
            VerdictStatus::Passed,
            VerdictStatus::Failed,
            VerdictStatus::Inconclusive,
            VerdictStatus::Error,
        ] {
            let receipt = receipt_with(verdict);
            assert_eq!(
                enforce_merge_bar(&receipt),
                gate_outcome(&receipt),
                "{verdict:?}"
            );
        }
    }

    #[test]
    fn met_bar_keeps_the_pass_exit_zero() {
        // All required checks Passed + required evidence present (replay root non-empty,
        // and the "receipt" kind is always satisfied) => the pass is kept.
        let receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![
                ("unit", VerdictStatus::Passed),
                ("build", VerdictStatus::Passed),
            ],
            vec!["unit", "build"],
            vec!["receipt", "replay"],
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.conclusion, "success");
        assert!(out.summary.contains("merge bar cleared"), "{}", out.summary);
    }

    #[test]
    fn required_check_present_and_failed_downgrades_to_exit_one() {
        // The bar was EXERCISED and not met => failure (exit 1), never a kept pass.
        let receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![
                ("unit", VerdictStatus::Passed),
                ("build", VerdictStatus::Failed),
            ],
            vec!["unit", "build"],
            vec![],
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 1);
        assert_eq!(out.conclusion, "failure");
        assert!(
            out.summary.contains("'build' did not pass"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn required_check_missing_downgrades_to_neutral_exit_two() {
        // A required check that does not appear at all => the bar was NOT exercised =>
        // neutral (exit 2), never a fabricated pass.
        let receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![("unit", VerdictStatus::Passed)],
            vec!["unit", "integration"],
            vec![],
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
        assert!(
            out.summary.contains("missing required check 'integration'"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn required_evidence_absent_downgrades_to_neutral_exit_two() {
        // "ledger" evidence required but the receipt pins no ledger_ref => neutral.
        let receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![("unit", VerdictStatus::Passed)],
            vec!["unit"],
            vec!["ledger"],
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
        assert!(
            out.summary.contains("required evidence 'ledger' absent"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn unknown_evidence_kind_is_fail_closed_neutral() {
        // An unrecognized evidence kind is UNSATISFIED (fail-closed, INV-R3) => neutral.
        let receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![("unit", VerdictStatus::Passed)],
            vec!["unit"],
            vec!["coverage-threshold"],
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
    }

    #[test]
    fn non_success_base_is_never_upgraded_even_if_bar_satisfied() {
        // A Failed base verdict whose co-sealed bar IS satisfied stays non-success — the
        // overlay may only keep/downgrade a pass, never upgrade (INV-R1, downgrade-only).
        let receipt = receipt_with_bar(
            VerdictStatus::Failed,
            vec![("unit", VerdictStatus::Passed)],
            vec!["unit"],
            vec!["receipt"],
        );
        let out = enforce_merge_bar(&receipt);
        let base = gate_outcome(&receipt);
        assert_eq!(out.exit_code, base.exit_code, "base failure exit preserved");
        assert_eq!(out.conclusion, base.conclusion);
        assert_ne!(
            out.exit_code, 0,
            "a satisfied bar never fabricates a pass on a Failed base"
        );
        // The base rationale is preserved and the bar report is APPENDED, not replaced.
        assert!(out.summary.starts_with(&base.summary), "{}", out.summary);
        assert!(out.summary.contains("merge bar cleared"), "{}", out.summary);
    }

    #[test]
    fn inconclusive_base_with_unmet_bar_stays_neutral_with_appended_report() {
        // An Inconclusive base (exit 2) keeps its exit; the bar report is appended.
        let receipt = receipt_with_bar(VerdictStatus::Inconclusive, vec![], vec!["unit"], vec![]);
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
        assert!(
            out.summary.contains("missing required check 'unit'"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn enforce_merge_bar_is_deterministic() {
        let receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![("unit", VerdictStatus::Passed)],
            vec!["unit"],
            vec!["receipt"],
        );
        assert_eq!(enforce_merge_bar(&receipt), enforce_merge_bar(&receipt));
    }

    #[test]
    fn policy_evidence_requires_a_pinned_policy_version() {
        // "policy" evidence is satisfied iff the receipt pinned a policy_version.
        let mut receipt = receipt_with_bar(
            VerdictStatus::Passed,
            vec![("unit", VerdictStatus::Passed)],
            vec!["unit"],
            vec!["policy"],
        );
        // No policy_version pinned => neutral.
        assert_eq!(enforce_merge_bar(&receipt).exit_code, 2);
        // Pin one => the bar clears.
        receipt.statement.provenance.policy_version = Some("pv1".to_string());
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.conclusion, "success");
    }

    // --- L3 checkspec-identity binding (frontier §1 fix #2) ---

    /// Pin the two sides of the checkspec-identity binding: the bar's
    /// `expected_checkspec_hash` (what the org authored the bar against) and the receipt's
    /// own `checkspec_hash` (what its checks were produced against).
    fn with_checkspec_binding(
        mut receipt: Receipt,
        expected: Option<&str>,
        actual: Option<&str>,
    ) -> Receipt {
        receipt.statement.merge_bar.expected_checkspec_hash = expected.map(str::to_owned);
        receipt.statement.checkspec_hash = actual.map(str::to_owned);
        receipt
    }

    #[test]
    fn checkspec_match_behaves_as_v14_name_matching() {
        // Bar pins checkspec X; the receipt was verified against X => name-matching
        // proceeds and a met bar still gates 0 (identical to pre-binding v14 behavior).
        let receipt = with_checkspec_binding(
            receipt_with_bar(
                VerdictStatus::Passed,
                vec![("unit", VerdictStatus::Passed)],
                vec!["unit"],
                vec!["receipt"],
            ),
            Some("spec-X"),
            Some("spec-X"),
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.conclusion, "success");
        assert!(out.summary.contains("merge bar cleared"), "{}", out.summary);
    }

    #[test]
    fn checkspec_mismatch_downgrades_a_pass_to_neutral() {
        // The receipt's checks were produced against a DIFFERENT checkspec than the bar was
        // authored against => the required-check names cannot be trusted => neutral (exit
        // 2), even though the named check "passes" (a stubbed `true` could impersonate it).
        let receipt = with_checkspec_binding(
            receipt_with_bar(
                VerdictStatus::Passed,
                vec![("unit", VerdictStatus::Passed)],
                vec!["unit"],
                vec![],
            ),
            Some("spec-real"),
            Some("spec-stub"),
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
        assert!(
            out.summary.contains("required checks cannot be trusted"),
            "{}",
            out.summary
        );
        // The short content addresses of both sides are surfaced for the human.
        assert!(out.summary.contains("spec-rea"), "{}", out.summary);
        assert!(out.summary.contains("spec-stu"), "{}", out.summary);
    }

    #[test]
    fn receipt_without_checkspec_under_a_pinning_bar_is_neutral() {
        // The bar pins a checkspec but the receipt pinned NONE => cannot prove the checks
        // ran against the authored checkspec => neutral (never a fabricated pass).
        let receipt = with_checkspec_binding(
            receipt_with_bar(
                VerdictStatus::Passed,
                vec![("unit", VerdictStatus::Passed)],
                vec!["unit"],
                vec![],
            ),
            Some("spec-real"),
            None,
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
        assert!(
            out.summary.contains("verified against none"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn bar_without_expected_checkspec_is_unchanged_name_matching() {
        // No `expected_checkspec_hash` => the checkspec gate is inert and v14 name-matching
        // governs, regardless of the receipt's own checkspec_hash (backward compatible).
        let receipt = with_checkspec_binding(
            receipt_with_bar(
                VerdictStatus::Passed,
                vec![("unit", VerdictStatus::Passed)],
                vec!["unit"],
                vec!["receipt"],
            ),
            None,
            Some("spec-anything"),
        );
        let out = enforce_merge_bar(&receipt);
        assert_eq!(out.exit_code, 0, "no pinned checkspec => by-name only");
        assert_eq!(out.conclusion, "success");
    }

    #[test]
    fn checkspec_mismatch_never_upgrades_a_failed_base() {
        // NEVER-UPGRADE: a Failed base verdict whose checkspec ALSO mismatches stays exit 1
        // — the checkspec gate is downgrade-only and can never promote a non-success base.
        let receipt = with_checkspec_binding(
            receipt_with_bar(
                VerdictStatus::Failed,
                vec![("unit", VerdictStatus::Passed)],
                vec!["unit"],
                vec![],
            ),
            Some("spec-real"),
            Some("spec-stub"),
        );
        let out = enforce_merge_bar(&receipt);
        let base = gate_outcome(&receipt);
        assert_eq!(out.exit_code, base.exit_code, "Failed base exit preserved");
        assert_ne!(
            out.exit_code, 0,
            "a mismatch never fabricates a pass on a Failed base"
        );
        assert!(
            out.summary.starts_with(&base.summary),
            "base rationale preserved, mismatch appended"
        );
        assert!(
            out.summary.contains("required checks cannot be trusted"),
            "{}",
            out.summary
        );
    }

    // --- L? isolation-tier floor (enforce_isolation_floor, INV-R7) ---

    /// A receipt with an explicit signed `provenance.isolation_tier`.
    fn receipt_with_isolation(verdict: VerdictStatus, tier: IsolationTier) -> Receipt {
        let mut receipt = receipt_with(verdict);
        receipt.statement.provenance.isolation_tier = tier;
        receipt
    }

    #[test]
    fn no_floor_is_pure_passthrough() {
        // `None` required tier never changes the outcome (default report-only behavior).
        for tier in [IsolationTier::Unconfined, IsolationTier::Hermetic] {
            let receipt = receipt_with_isolation(VerdictStatus::Passed, tier);
            let base = gate_outcome(&receipt);
            assert_eq!(enforce_isolation_floor(base.clone(), &receipt, None), base);
        }
    }

    #[test]
    fn met_floor_keeps_the_pass() {
        // actual >= required => unchanged. Contained satisfies a Contained floor, and the
        // stronger Hermetic satisfies it too.
        for tier in [IsolationTier::Contained, IsolationTier::Hermetic] {
            let receipt = receipt_with_isolation(VerdictStatus::Passed, tier);
            let base = gate_outcome(&receipt);
            let out = enforce_isolation_floor(base, &receipt, Some(IsolationTier::Contained));
            assert_eq!(out.exit_code, 0, "{tier:?} clears a Contained floor");
            assert_eq!(out.conclusion, "success");
        }
    }

    #[test]
    fn unmet_floor_downgrades_a_pass_to_neutral() {
        // A Contained receipt under a Hermetic floor: the re-run was not bit-for-bit, so
        // the pass is downgraded to neutral (exit 2) — never a fabricated pass (INV-R7).
        let receipt = receipt_with_isolation(VerdictStatus::Passed, IsolationTier::Contained);
        let base = gate_outcome(&receipt);
        let out = enforce_isolation_floor(base, &receipt, Some(IsolationTier::Hermetic));
        assert_eq!(out.exit_code, 2);
        assert_eq!(out.conclusion, "neutral");
        assert!(
            out.summary
                .contains("isolation tier contained below required hermetic"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn unmet_floor_never_upgrades_a_failed_base() {
        // NEVER-UPGRADE: a Failed base whose tier ALSO falls short stays exit 1 — the
        // floor is downgrade-only and can never promote a non-success base.
        let receipt = receipt_with_isolation(VerdictStatus::Failed, IsolationTier::Unconfined);
        let base = gate_outcome(&receipt);
        let out = enforce_isolation_floor(base.clone(), &receipt, Some(IsolationTier::Hermetic));
        assert_eq!(out.exit_code, base.exit_code, "Failed base exit preserved");
        assert_ne!(out.exit_code, 0, "a tier shortfall never fabricates a pass");
        assert!(
            out.summary.starts_with(&base.summary),
            "base rationale preserved, shortfall appended"
        );
        assert!(
            out.summary.contains("below required hermetic"),
            "{}",
            out.summary
        );
    }

    #[test]
    fn isolation_floor_is_deterministic() {
        let receipt = receipt_with_isolation(VerdictStatus::Passed, IsolationTier::Contained);
        let base = gate_outcome(&receipt);
        assert_eq!(
            enforce_isolation_floor(base.clone(), &receipt, Some(IsolationTier::Hermetic)),
            enforce_isolation_floor(base, &receipt, Some(IsolationTier::Hermetic)),
        );
    }

    #[test]
    fn gate_outcome_serde_round_trip() {
        let out = gate_outcome(&receipt_with(VerdictStatus::Failed));
        let json = serde_json::to_string(&out).expect("serializes");
        let back: GateOutcome = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(out, back);
        // Field names are stable / snake_case as the integrator's golden expects.
        assert!(json.contains("\"exit_code\""));
        assert!(json.contains("\"conclusion\""));
        assert!(json.contains("\"summary\""));
    }
}
