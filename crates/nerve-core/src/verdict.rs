//! Pure, golden-tested helpers for the L2 **execution-grounded verdict**
//! (`docs/designs/trust-substrate.md` §3 L2, INV-R1). Given the
//! [`nerve_proto::verdict`] shapes — the org's own checks re-run in a sealed
//! closure — this module hashes individual [`CheckResult`]s, aggregates them
//! into a [`VerdictStatus`] under a caller-supplied *required* mask, derives a
//! content address for the whole [`Verdict`], and seals it.
//!
//! **Court reporter, not judge (INV-R1).** Aggregation never claims the code is
//! "correct"; it reports whether the org's own bar was cleared. The required
//! mask says which checks gate the verdict; advisory checks are recorded but
//! cannot fail it. A `Flaky` required check yields [`VerdictStatus::Inconclusive`]
//! (the bar was not cleanly exercised), and an `Error` yields
//! [`VerdictStatus::Error`] (the check could not be run at all).
//!
//! **Determinism boundary's L2 brick:** every function here is a pure function
//! of its arguments — no IO, no wall-clock, no randomness. `verified_at_ms` is a
//! host-supplied timestamp carried for display and **never** hashed, so the same
//! checks in yield a byte-identical content address out (INV-R2). SHA-256 (via
//! `sha2`) is used for the audit trail, mirroring [`crate::provenance`].

// Re-export the shapes this module operates on so a consumer of the kernel builds
// and reads verdicts through `nerve_core` alone, without its own `nerve-proto` dep.
pub use nerve_proto::verdict::{
    CheckKind, CheckResult, CheckStatus, VERDICT_SCHEMA_VERSION, Verdict, VerdictStatus,
};
use sha2::{Digest, Sha256};

/// Lowercase-hex SHA-256 of one check's canonical JSON. Deterministic: a
/// [`CheckResult`] is a fixed-field struct with **no maps and no floats**, so
/// `serde_json` emits byte-stable bytes (INV-R2). The host-supplied
/// `duration_ms` is part of the check's recorded payload and is hashed in-band
/// here (a check's *evidence* includes how long it took); only the verdict-level
/// `verified_at_ms` is excluded from the verdict content id.
#[must_use]
pub fn hash_check(check: &CheckResult) -> String {
    let bytes = serde_json::to_vec(check).expect("CheckResult serializes infallibly");
    hex(Sha256::digest(bytes).as_slice())
}

/// Aggregate per-check statuses into a single [`VerdictStatus`] under the
/// `required` mask. `required[i]` gates `checks[i]`; a shorter/over-long mask is
/// read positionally with missing entries treated as **not required** (advisory).
///
/// Rules (court reporter, not judge — INV-R1):
/// - **No required check exercised** (empty checks, or every check advisory) →
///   [`VerdictStatus::Inconclusive`]: the org's bar was never run, so no
///   clearance can be claimed.
/// - A required check is `Error` → [`VerdictStatus::Error`] (could not be run).
/// - A required check is `Fail` → [`VerdictStatus::Failed`] (bar not cleared).
/// - A required check is `Flaky` → [`VerdictStatus::Inconclusive`] (bar not
///   cleanly exercised).
/// - Otherwise every required check `Pass`ed → [`VerdictStatus::Passed`].
///
/// `Error` dominates `Failed`, which dominates `Inconclusive`; advisory checks
/// never affect the verdict.
#[must_use]
pub fn aggregate_status(checks: &[CheckResult], required: &[bool]) -> VerdictStatus {
    let mut any_required = false;
    let mut saw_error = false;
    let mut saw_fail = false;
    let mut saw_flaky = false;
    for (i, check) in checks.iter().enumerate() {
        if !required.get(i).copied().unwrap_or(false) {
            continue; // advisory check: recorded, never load-bearing.
        }
        any_required = true;
        match check.status {
            CheckStatus::Error => saw_error = true,
            CheckStatus::Fail => saw_fail = true,
            CheckStatus::Flaky => saw_flaky = true,
            CheckStatus::Pass => {}
        }
    }
    if !any_required {
        return VerdictStatus::Inconclusive;
    }
    if saw_error {
        VerdictStatus::Error
    } else if saw_fail {
        VerdictStatus::Failed
    } else if saw_flaky {
        VerdictStatus::Inconclusive
    } else {
        VerdictStatus::Passed
    }
}

/// Content address for a [`Verdict`]: lowercase-hex SHA-256 over the verdict's
/// load-bearing identity — the run it grounds, the diff it covers, the checkspec
/// and closure it ran against, and the per-check digests in order. The
/// host-supplied `verified_at_ms` and the verdict's own derived ids are **not**
/// folded in, so re-verifying the same checks against the same closure yields a
/// reproducible id (INV-R2). Newline field separators keep the pre-image
/// unambiguous across variable-length parts.
#[must_use]
pub fn verdict_content_id(
    run_id: &str,
    diff_hash: Option<&str>,
    checkspec_hash: &str,
    closure_digest: &str,
    checks: &[CheckResult],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(run_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(diff_hash.unwrap_or("").as_bytes());
    hasher.update(b"\n");
    hasher.update(checkspec_hash.as_bytes());
    hasher.update(b"\n");
    hasher.update(closure_digest.as_bytes());
    for check in checks {
        hasher.update(b"\n");
        hasher.update(hash_check(check).as_bytes());
    }
    hex(hasher.finalize().as_slice())
}

/// Seal a set of re-run [`CheckResult`]s into a content-addressed [`Verdict`].
/// `status` is computed by [`aggregate_status`] under `required`; `verdict_id`
/// and `verdict_hash` are both set to [`verdict_content_id`] (at L2 the verdict's
/// identity *is* its content address). `verified_at_ms` is host metadata carried
/// for display and is **never** part of the content address.
#[must_use]
pub fn build_verdict(
    run_id: impl Into<String>,
    diff_hash: Option<String>,
    checkspec_hash: impl Into<String>,
    closure_digest: impl Into<String>,
    checks: Vec<CheckResult>,
    required: &[bool],
    verified_at_ms: u64,
) -> Verdict {
    let run_id = run_id.into();
    let checkspec_hash = checkspec_hash.into();
    let closure_digest = closure_digest.into();
    let status = aggregate_status(&checks, required);
    let content_id = verdict_content_id(
        &run_id,
        diff_hash.as_deref(),
        &checkspec_hash,
        &closure_digest,
        &checks,
    );
    Verdict {
        schema_version: VERDICT_SCHEMA_VERSION,
        verdict_id: content_id.clone(),
        run_id,
        diff_hash,
        status,
        checkspec_hash,
        closure_digest,
        checks,
        verified_at_ms,
        verdict_hash: content_id,
    }
}

/// Lowercase-hex SHA-256 of a checkspec's canonical JSON — the stable identity of
/// "which checks, run how" that a verdict commits to via its `checkspec_hash`.
/// The caller passes the spec as a [`serde_json::Value`]; deterministic only when
/// the value contains no floats and `serde_json` preserves object key order
/// (it sorts keys for `Value::Object`, so this is order-stable).
#[must_use]
pub fn hash_checkspec(spec_json: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(spec_json).expect("Value serializes infallibly");
    hex(Sha256::digest(bytes).as_slice())
}

/// Lowercase-hex encode bytes (no allocation per byte beyond the result string).
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

    fn check(name: &str, kind: CheckKind, status: CheckStatus) -> CheckResult {
        CheckResult {
            name: name.into(),
            kind,
            status,
            reproducible: true,
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 10,
            output_hash: String::new(),
            runs: 1,
            passed: 1,
        }
    }

    #[test]
    fn hash_check_is_stable_and_distinguishes_content() {
        let a = check("test", CheckKind::Test, CheckStatus::Pass);
        assert_eq!(hash_check(&a), hash_check(&a), "same check -> same hash");
        assert_eq!(hash_check(&a).len(), 64);
        assert!(hash_check(&a).chars().all(|c| c.is_ascii_hexdigit()));
        // A different status yields a different digest.
        let b = check("test", CheckKind::Test, CheckStatus::Fail);
        assert_ne!(hash_check(&a), hash_check(&b));
        // A different name yields a different digest.
        let c = check("lint", CheckKind::Test, CheckStatus::Pass);
        assert_ne!(hash_check(&a), hash_check(&c));
    }

    #[test]
    fn aggregate_empty_or_all_advisory_is_inconclusive() {
        // No checks at all: the org's bar was never exercised.
        assert_eq!(aggregate_status(&[], &[]), VerdictStatus::Inconclusive);
        // A failing check that is NOT required cannot fail the verdict.
        let advisory = vec![check("a", CheckKind::Lint, CheckStatus::Fail)];
        assert_eq!(
            aggregate_status(&advisory, &[false]),
            VerdictStatus::Inconclusive
        );
        // A short mask leaves the check advisory by default.
        assert_eq!(
            aggregate_status(&advisory, &[]),
            VerdictStatus::Inconclusive
        );
    }

    #[test]
    fn aggregate_all_required_pass_is_passed() {
        let checks = vec![
            check("test", CheckKind::Test, CheckStatus::Pass),
            check("build", CheckKind::Build, CheckStatus::Pass),
        ];
        assert_eq!(
            aggregate_status(&checks, &[true, true]),
            VerdictStatus::Passed
        );
    }

    #[test]
    fn aggregate_required_failure_dominates_and_advisory_ignored() {
        let checks = vec![
            check("test", CheckKind::Test, CheckStatus::Pass),
            check("lint", CheckKind::Lint, CheckStatus::Fail),
        ];
        // lint required -> Failed.
        assert_eq!(
            aggregate_status(&checks, &[true, true]),
            VerdictStatus::Failed
        );
        // lint advisory -> the failure is ignored, verdict Passes.
        assert_eq!(
            aggregate_status(&checks, &[true, false]),
            VerdictStatus::Passed
        );
    }

    #[test]
    fn aggregate_flaky_required_is_inconclusive_error_dominates() {
        let flaky = vec![check("test", CheckKind::Test, CheckStatus::Flaky)];
        assert_eq!(
            aggregate_status(&flaky, &[true]),
            VerdictStatus::Inconclusive
        );
        // Error dominates a flaky AND a failure when both are required.
        let mixed = vec![
            check("test", CheckKind::Test, CheckStatus::Flaky),
            check("build", CheckKind::Build, CheckStatus::Fail),
            check("typecheck", CheckKind::Typecheck, CheckStatus::Error),
        ];
        assert_eq!(
            aggregate_status(&mixed, &[true, true, true]),
            VerdictStatus::Error
        );
    }

    #[test]
    fn verdict_content_id_is_deterministic_and_field_sensitive() {
        let checks = vec![check("test", CheckKind::Test, CheckStatus::Pass)];
        let id = verdict_content_id("run-1", Some("diff-a"), "spec-1", "closure-1", &checks);
        assert_eq!(id.len(), 64);
        assert_eq!(
            id,
            verdict_content_id("run-1", Some("diff-a"), "spec-1", "closure-1", &checks),
            "same input -> same id"
        );
        // Each load-bearing component perturbs the id.
        assert_ne!(
            id,
            verdict_content_id("run-2", Some("diff-a"), "spec-1", "closure-1", &checks)
        );
        assert_ne!(
            id,
            verdict_content_id("run-1", Some("diff-b"), "spec-1", "closure-1", &checks)
        );
        assert_ne!(
            id,
            verdict_content_id("run-1", Some("diff-a"), "spec-2", "closure-1", &checks)
        );
        assert_ne!(
            id,
            verdict_content_id("run-1", Some("diff-a"), "spec-1", "closure-2", &checks)
        );
        // A changed check perturbs the id; missing diff differs from a present one.
        let other = vec![check("test", CheckKind::Test, CheckStatus::Fail)];
        assert_ne!(
            id,
            verdict_content_id("run-1", Some("diff-a"), "spec-1", "closure-1", &other)
        );
        assert_ne!(
            id,
            verdict_content_id("run-1", None, "spec-1", "closure-1", &checks)
        );
    }

    #[test]
    fn build_verdict_seals_status_and_addresses_by_content() {
        let checks = vec![
            check("test", CheckKind::Test, CheckStatus::Pass),
            check("build", CheckKind::Build, CheckStatus::Pass),
        ];
        let v = build_verdict(
            "run-1",
            Some("diff-a".into()),
            "spec-1",
            "closure-1",
            checks.clone(),
            &[true, true],
            1000,
        );
        assert_eq!(v.schema_version, VERDICT_SCHEMA_VERSION);
        assert_eq!(v.status, VerdictStatus::Passed);
        assert_eq!(v.verdict_id, v.verdict_hash, "id is the content address");
        assert_eq!(
            v.verdict_id,
            verdict_content_id("run-1", Some("diff-a"), "spec-1", "closure-1", &checks)
        );
        // verified_at_ms is display-only: a different timestamp does not change the id.
        let v2 = build_verdict(
            "run-1",
            Some("diff-a".into()),
            "spec-1",
            "closure-1",
            checks,
            &[true, true],
            999_999,
        );
        assert_eq!(v.verdict_id, v2.verdict_id);
        assert_eq!(v.verdict_hash, v2.verdict_hash);
    }

    #[test]
    fn build_verdict_records_status_inconclusive_for_empty_checks() {
        let v = build_verdict("run-1", None, "spec-1", "closure-1", vec![], &[], 1000);
        assert_eq!(v.status, VerdictStatus::Inconclusive);
        assert!(v.checks.is_empty());
        assert_eq!(v.verdict_id.len(), 64);
    }

    #[test]
    fn hash_checkspec_is_stable_and_key_order_independent() {
        let a = serde_json::json!({"checks": ["test", "build"], "reruns": 3});
        let b = serde_json::json!({"reruns": 3, "checks": ["test", "build"]});
        assert_eq!(hash_checkspec(&a), hash_checkspec(&a));
        // serde_json sorts object keys, so reordered specs hash identically.
        assert_eq!(hash_checkspec(&a), hash_checkspec(&b));
        // A changed value perturbs the hash.
        let c = serde_json::json!({"checks": ["test"], "reruns": 3});
        assert_ne!(hash_checkspec(&a), hash_checkspec(&c));
    }
}
