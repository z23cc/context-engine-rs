//! The L2 **CLI re-verify flow** (`docs/designs/trust-substrate.md` §3 L2 / §8.4,
//! INV-R1) — the in-process half of `nerve verify`. Given a captured run id and the
//! workspace root, it re-runs the org's own checks (`<root>/.nerve/checks.json`) in the
//! recorded closure via the SAME engine the daemon uses
//! ([`crate::verify_runner::handle_verify_start`]), seals the borrowed [`Verdict`],
//! appends it to the L1 ledger, and issues a signed L4 Verification [`Receipt`] under
//! `<root>/.nerve/receipts/` — all through the one canonical
//! [`crate::verify_runner::seal_and_attest`] tail (no forked copy, INV-R1).
//!
//! **Court reporter, not judge (INV-R1).** This never decides "correct": it re-runs the
//! org's declared bar and seals what it saw. A missing `checks.json` exercises no
//! required bar, so the verdict is honestly `Inconclusive` (the gate maps it to exit 2)
//! — NEVER a fabricated pass. Re-running, signing, and IO live here above the
//! determinism boundary (INV-R2); the aggregation/hashing/canonicalization they call are
//! pure in `nerve-core`.

use crate::ledger_store::LedgerStore;
use crate::receipt_store::ReceiptStore;
use crate::run_store::RunStore;
use crate::sandbox::{ProcessLauncher, SandboxLauncher};
use crate::verify_runner::{AttestStores, handle_verify_start, seal_and_attest};
use crate::verify_store::VerifyStore;
use anyhow::{Context, Result, anyhow};
use nerve_core::CancelToken;
use nerve_core::receipt::Receipt;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Re-verify a captured run in-process and return its freshly sealed, signed Receipt.
///
/// Wires the four `.nerve/` stores for `root`, builds a best-effort [`ProcessLauncher`],
/// re-runs the org's checks via [`handle_verify_start`] (folding a sealed [`Verdict`]),
/// then runs the canonical [`seal_and_attest`] tail (ledger append + signed receipt
/// persist) and returns the persisted receipt. An unknown `run_id` is a hard error; a
/// missing `checks.json` yields an honest `Inconclusive` receipt (never a fabricated
/// pass — INV-R1).
pub(crate) fn run_verify_flow(root: &Path, run_id: &str, reruns: Option<u32>) -> Result<Receipt> {
    let run_store = RunStore::for_scope(Some(root)).context("open run store")?;
    let verify_store = VerifyStore::for_scope(Some(root)).context("open verdict store")?;
    let receipt_store = ReceiptStore::for_scope(Some(root)).context("open receipt store")?;
    let ledger_store = LedgerStore::for_scope(Some(root)).context("open ledger store")?;
    let launcher: Arc<dyn SandboxLauncher> = Arc::new(ProcessLauncher);

    let verdict = handle_verify_start(
        Some(&run_store),
        Some(&verify_store),
        &launcher,
        root,
        run_id,
        reruns,
        None,
        &CancelToken::never(),
        now_ms(),
    )
    .map_err(|err| anyhow!("re-verify run `{run_id}`: {err}"))?;

    let run = run_store
        .load_record(run_id)
        .with_context(|| format!("reload captured run `{run_id}`"))?;
    let signer = crate::signer::local_signer(Some(root));
    let stores = AttestStores {
        ledger: Some(&ledger_store),
        receipt: Some(&receipt_store),
    };
    // L3 (INV-R5): co-seal the org's in-force merge bar into the receipt statement so the
    // gate enforces the bar the receipt SIGNED — never a gate-side policy re-read. An
    // absent/empty policy-plane.json embeds nothing (policy_version stays None), so the
    // receipt is byte-identical to pre-L3 (no golden churn).
    let bar = crate::policy_plane::PolicyPlane::resolve(Some(root)).sealed_bar();
    // Stamp the PROBED containment tier of the launcher that ran this verify re-run into
    // the signed receipt (INV-R7) — today's best-effort `ProcessLauncher` is `Contained`.
    seal_and_attest(
        &run,
        &verdict,
        &stores,
        &bar,
        &signer,
        launcher.isolation_tier(),
        now_ms(),
    )
    .receipt
    .ok_or_else(|| anyhow!("failed to seal/persist a Verification Receipt for run `{run_id}`"))
}

/// Wall-clock millis since the epoch — the host-supplied issuance timestamp threaded
/// into the (otherwise pure) seal pipeline. Lives here above the determinism boundary,
/// never inside a hashed `nerve-core` value (INV-R2).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::provenance::{Event, EventKind, RunInputs};
    use nerve_core::receipt_gate::gate_outcome;
    use nerve_core::verdict::VerdictStatus;
    use tempfile::tempdir;

    /// Seed one captured run under `<root>/.nerve/runs` and return its content-address id.
    fn seed_run(root: &Path) -> String {
        let store = RunStore::for_scope(Some(root)).unwrap();
        let run = nerve_core::build_run(
            "job-verify",
            "codex",
            Some(root.display().to_string()),
            1,
            Some(2),
            true,
            vec![Event {
                seq: 0,
                kind: EventKind::RunStarted {
                    agent: "codex".into(),
                    task: "t".into(),
                    cwd: None,
                    inputs: None,
                },
            }],
            RunInputs::default(),
        );
        store.write_record(&run).unwrap();
        run.run_id
    }

    /// Write `<root>/.nerve/checks.json` with a single required check running `program`.
    fn write_one_check(root: &Path, program: &str) {
        let dir = root.join(".nerve");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("checks.json"),
            serde_json::json!({"checks":[
                {"name":"smoke","kind":"test","command":program,"required":true}
            ]})
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn passing_check_seals_a_passed_receipt_that_gates_zero() {
        let dir = tempdir().unwrap();
        let run_id = seed_run(dir.path());
        // `true` exits 0 — the org's bar is cleared.
        write_one_check(dir.path(), "true");

        let receipt = run_verify_flow(dir.path(), &run_id, Some(1)).unwrap();
        assert_eq!(receipt.statement.verdict, VerdictStatus::Passed);
        assert_eq!(receipt.statement.provenance.run_id, run_id);
        assert_eq!(gate_outcome(&receipt).exit_code, 0);

        // The receipt was actually persisted under <root>/.nerve/receipts and reloads.
        let store = ReceiptStore::for_scope(Some(dir.path())).unwrap();
        let loaded = store.load_record(&receipt.receipt_id).unwrap();
        assert_eq!(loaded.receipt_id, receipt.receipt_id);
    }

    #[test]
    fn failing_check_seals_a_failed_receipt_that_gates_one() {
        let dir = tempdir().unwrap();
        let run_id = seed_run(dir.path());
        // `false` exits non-zero — a required check fails the org's bar.
        write_one_check(dir.path(), "false");

        let receipt = run_verify_flow(dir.path(), &run_id, Some(1)).unwrap();
        assert_eq!(receipt.statement.verdict, VerdictStatus::Failed);
        assert_eq!(gate_outcome(&receipt).exit_code, 1);
    }

    #[test]
    fn missing_checks_json_is_inconclusive_never_fabricates_a_pass() {
        let dir = tempdir().unwrap();
        let run_id = seed_run(dir.path());
        // No checks.json at all -> no required bar exercised.
        let receipt = run_verify_flow(dir.path(), &run_id, None).unwrap();
        assert_eq!(receipt.statement.verdict, VerdictStatus::Inconclusive);
        // The gate maps an inconclusive receipt to neutral exit 2 (INV-R1).
        assert_eq!(gate_outcome(&receipt).exit_code, 2);
    }

    #[test]
    fn unknown_run_is_a_hard_error() {
        let dir = tempdir().unwrap();
        write_one_check(dir.path(), "true");
        let err = run_verify_flow(dir.path(), "no-such-run", Some(1)).unwrap_err();
        assert!(err.to_string().contains("no-such-run"), "{err}");
    }

    /// L3 (INV-R5): a run verified under a real `<root>/.nerve/policy-plane.json` co-seals
    /// the org's merge bar + the pinned `policy_version` into the receipt statement, and
    /// the bar named matches the co-sealed check (here `smoke`, which passes) so the gate
    /// clears it. An empty/absent policy embeds nothing (covered elsewhere).
    #[test]
    fn verify_embeds_the_in_force_bar_and_policy_version() {
        use nerve_core::receipt_gate::enforce_merge_bar;
        let dir = tempdir().unwrap();
        let run_id = seed_run(dir.path());
        write_one_check(dir.path(), "true"); // the check is named "smoke" + passes

        // A real policy plane requiring the "smoke" check + a receipt evidence predicate.
        let nerve_dir = dir.path().join(".nerve");
        std::fs::create_dir_all(&nerve_dir).unwrap();
        std::fs::write(
            nerve_dir.join("policy-plane.json"),
            serde_json::json!({
                "schema_version": 1,
                "merge_bar": { "required_checks": ["smoke"] },
                "required_evidence": [{ "kind": "receipt" }]
            })
            .to_string(),
        )
        .unwrap();

        let receipt = run_verify_flow(dir.path(), &run_id, Some(1)).unwrap();
        // The bar + a non-empty policy_version were embedded into the SIGNED statement.
        assert_eq!(receipt.statement.merge_bar.required_checks, vec!["smoke"]);
        assert_eq!(receipt.statement.required_evidence.len(), 1);
        assert!(
            receipt.statement.provenance.policy_version.is_some(),
            "a non-empty policy pins its version"
        );
        // The co-sealed bar is met (smoke passed + receipt evidence present) -> gates 0.
        assert_eq!(enforce_merge_bar(&receipt).exit_code, 0);
    }
}
