//! Durable L4 receipt persistence (`docs/designs/trust-substrate.md` §8, INV-R1) — the
//! sibling of [`RunStore`](crate::run_store) and [`DelegateStore`](crate::delegate_store).
//! A captured [`Run`] (L0) is sealed into a portable, signed **Verification Receipt**:
//! the pure `nerve-core` machinery builds + canonicalizes the statement and wraps it in
//! a DSSE PAE, the impure [`Signer`] seam signs those bytes, and the resulting
//! content-addressed [`Receipt`] is persisted here so it can be fetched (`receipt.get`)
//! and later landed as a GitHub/GitLab merge-gate.
//!
//! ```text
//! .nerve/receipts/<receipt_id>.json   # the versioned Receipt (receipt_id == content address)
//! ```
//!
//! Mirrors the verified [`RunStore`] discipline — a versioned record (the receipt's
//! own `schema_version`, a tolerant [`load_record`](ReceiptStore::load_record) path,
//! and a [`migrate_to_current`] seam owned by THIS module), atomic writes (temp +
//! rename), and **best-effort** issuance: a persistence failure NEVER fails the
//! delegated turn (a receipt is an audit artifact, not a gate on generation).
//! Canonicalization, hashing, and verdict aggregation are pure and live in
//! `nerve-core::receipt`; only signing + IO live here, above the determinism boundary.

use crate::signer::Signer;
use anyhow::{Context, Result, anyhow};
use nerve_core::provenance::Run;
use nerve_core::receipt::{
    LedgerRef, RECEIPT_PREDICATE_TYPE, RECEIPT_SCHEMA_VERSION, Receipt, ReceiptCheck,
    ReceiptSignature,
};
use nerve_core::verdict::VerdictStatus;
use nerve_runtime::RuntimeError;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The identity of a freshly issued + persisted receipt — returned by
/// [`issue_receipt_for_run`] so the host can announce it via
/// [`RuntimeEvent::receipt_issued`](nerve_runtime::RuntimeEvent).
pub(crate) struct IssuedReceipt {
    /// The receipt's content address (== `statement_id` over the canonical statement).
    pub(crate) receipt_id: String,
    /// The run the receipt attests to (echoed for the announcing event).
    pub(crate) run_id: String,
    /// The aggregated org's-own-test verdict carried by the receipt.
    pub(crate) verdict: VerdictStatus,
}

/// A directory of persisted issued receipts (`<dir>/<receipt_id>.json`). Sibling of
/// [`RunStore`](crate::run_store).
#[derive(Clone)]
pub(crate) struct ReceiptStore {
    dir: PathBuf,
}

impl ReceiptStore {
    /// Wrap an explicit receipts directory.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Resolve the receipts directory for a scope: `<root>/.nerve/receipts` for a
    /// project root, else the global `config_home()/receipts`.
    pub(crate) fn for_scope(root: Option<&Path>) -> Result<Self> {
        Ok(Self::new(resolve_receipts_dir(root)?))
    }

    /// The backing directory (mirrors `RunStore::dir`; used by tests).
    #[allow(dead_code, reason = "accessor mirroring RunStore::dir; used by tests")]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The per-receipt file `<dir>/<receipt_id>.json` (validating the id stays in-dir).
    fn path_for(&self, receipt_id: &str) -> Result<PathBuf> {
        validate_id(receipt_id)?;
        Ok(self.dir.join(format!("{receipt_id}.json")))
    }

    /// Persist a receipt atomically (temp + rename), creating the dir on demand.
    pub(crate) fn write_record(&self, receipt: &Receipt) -> Result<()> {
        let path = self.path_for(&receipt.receipt_id)?;
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create receipts dir {}", self.dir.display()))?;
        let json = serde_json::to_string_pretty(receipt).context("serialize receipt")?;
        atomic_write(&path, json.as_bytes())
    }

    /// Load and migrate one receipt by id.
    pub(crate) fn load_record(&self, receipt_id: &str) -> Result<Receipt> {
        let path = self.path_for(receipt_id)?;
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        deserialize_record(&raw).with_context(|| format!("failed to parse receipt {receipt_id}"))
    }

    /// All persisted receipts, most recent first (tolerating a missing dir + bad files).
    /// Consumed by the deferred `nerve_receipt` MCP list + `receipt.list` finalization.
    #[allow(dead_code)]
    pub(crate) fn list(&self) -> Result<Vec<Receipt>> {
        let mut receipts = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(receipts),
            Err(err) => return Err(anyhow!("failed to read {}: {err}", self.dir.display())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(receipt) = deserialize_record(&raw) {
                receipts.push(receipt);
            }
        }
        receipts.sort_by(|a, b| {
            b.statement
                .issued_at_ms
                .cmp(&a.statement.issued_at_ms)
                .then_with(|| b.receipt_id.cmp(&a.receipt_id))
        });
        Ok(receipts)
    }
}

/// Issue a signed Verification Receipt for a captured [`Run`] and persist it.
///
/// Threads the pure `nerve-core` pipeline — [`build_statement`](nerve_core::receipt::build_statement)
/// → canonical bytes → [`dsse_pae`](nerve_core::receipt::dsse_pae) → `signer.sign(pae)`
/// → [`seal_receipt`](nerve_core::receipt::seal_receipt) — then writes the sealed
/// receipt to `store`. Pure throughout except for the injected `signer` and the
/// `store` IO; `issued_at_ms` is host-supplied (never `now()` inside a hashed value),
/// so the statement's content address stays reproducible for a fixed clock + key.
///
/// **Best-effort:** a missing store or a write failure yields `None` and never
/// propagates into the delegated turn. Empty `checks` honestly aggregates to
/// [`VerdictStatus::Inconclusive`] (no bar exercised — INV-R1), not a fabricated pass.
#[allow(clippy::too_many_arguments)] // reason: 1:1 binding of the receipt's provenance fields
pub(crate) fn issue_receipt_for_run(
    run: &Run,
    checks: Vec<ReceiptCheck>,
    toolchain_digest: Option<String>,
    policy_version: Option<String>,
    ledger_ref: Option<LedgerRef>,
    issued_at_ms: u64,
    signer: &dyn Signer,
    store: Option<&ReceiptStore>,
) -> Option<IssuedReceipt> {
    let statement = nerve_core::receipt::build_statement(
        run,
        checks,
        toolchain_digest,
        policy_version,
        ledger_ref,
        issued_at_ms,
    );
    let payload = nerve_core::receipt::canonical_statement_bytes(&statement);
    let pae = nerve_core::receipt::dsse_pae(RECEIPT_PREDICATE_TYPE, &payload);
    let (sig, public_key) = signer.sign(&pae);
    let signature = ReceiptSignature {
        payload_type: RECEIPT_PREDICATE_TYPE.to_string(),
        backend: signer.backend().to_string(),
        keyid: signer.keyid(),
        sig,
        public_key: Some(public_key),
        bundle: None,
    };
    let receipt = nerve_core::receipt::seal_receipt(statement, signature);
    let issued = IssuedReceipt {
        receipt_id: receipt.receipt_id.clone(),
        run_id: receipt.statement.provenance.run_id.clone(),
        verdict: receipt.statement.verdict,
    };
    let store = store?;
    match store.write_record(&receipt) {
        Ok(()) => Some(issued),
        Err(_) => None,
    }
}

/// Resolve a `receipt.get`: the full sealed [`Receipt`] by id. An unknown id (or no
/// served root) is an error, mirroring `run.get`.
pub(crate) fn run_receipt_get(
    receipt_id: &str,
    store: Option<&ReceiptStore>,
) -> Result<Value, RuntimeError> {
    let store = store.ok_or_else(|| RuntimeError::adapter(format!("no receipt `{receipt_id}`")))?;
    let receipt = store
        .load_record(receipt_id)
        .map_err(|err| RuntimeError::adapter(format!("no receipt `{receipt_id}`: {err}")))?;
    let receipt = serde_json::to_value(&receipt).map_err(|err| {
        RuntimeError::adapter(format!("failed to render receipt `{receipt_id}`: {err}"))
    })?;
    Ok(json!({ "receipt": receipt }))
}

/// Atomic file write: temp file + rename, so a reader never observes a half-written
/// file. `rename` is atomic within a directory on the platforms we target.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("receipt-write")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Parse + migrate a receipt, tolerant of an older/missing `schema_version` (treated
/// as v1); rejects a newer-than-supported version.
fn deserialize_record(raw: &str) -> Result<Receipt> {
    let mut value: Value = serde_json::from_str(raw).context("invalid receipt JSON")?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    migrate_to_current(&mut value, version)?;
    serde_json::from_value(value).context("receipt shape mismatch")
}

/// Upgrade a receipt `value` from `version` to [`RECEIPT_SCHEMA_VERSION`] in place.
/// Only one version exists today, so this is the newer-than-known guard + a re-stamp;
/// add an arm per future bump (mirrors `RunStore` / `DelegateStore`).
fn migrate_to_current(value: &mut Value, version: u64) -> Result<()> {
    if version > u64::from(RECEIPT_SCHEMA_VERSION) {
        return Err(anyhow!(
            "receipt schema_version {version} is newer than supported {RECEIPT_SCHEMA_VERSION}; upgrade nerve"
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("schema_version".into(), Value::from(RECEIPT_SCHEMA_VERSION));
    }
    Ok(())
}

/// `<root>/.nerve/receipts` for a project root, else the global `config_home()/receipts`.
fn resolve_receipts_dir(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root.join(".nerve").join("receipts")),
        None => {
            let home = nerve_agent::auth::config_home().map_err(|err| anyhow!("{err}"))?;
            Ok(home.join("receipts"))
        }
    }
}

/// Reject ids that could escape the receipts directory (same token rule as the other
/// stores: ASCII alphanumerics plus `-`/`_`). A content-address receipt id is hex, so
/// it always passes; this guards against a malformed/empty id reaching the filesystem.
fn validate_id(id: &str) -> Result<()> {
    let valid = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if valid {
        Ok(())
    } else {
        Err(anyhow!(
            "invalid receipt id '{id}': use only letters, digits, '-' and '_'"
        ))
    }
}

/// Wall-clock millis since the epoch — the host-supplied issuance timestamp the caller
/// passes into [`issue_receipt_for_run`]. Lives here (the impure store), NEVER inside a
/// `nerve-core` pure helper.
#[allow(
    dead_code,
    reason = "host clock for callers that issue at 'now'; tests pass a fixed time"
)]
pub(crate) fn now_ms() -> u64 {
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
    use crate::signer::{LocalEd25519Signer, ed25519_verify};
    use nerve_core::provenance::{Event, EventKind, RunInputs};
    use nerve_core::verdict::CheckKind;
    use tempfile::tempdir;

    fn sample_run(task: &str) -> Run {
        nerve_core::build_run(
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
                    task: task.into(),
                    cwd: Some("/repo".into()),
                    inputs: None,
                },
            }],
            RunInputs::default(),
        )
    }

    fn passing_check() -> ReceiptCheck {
        ReceiptCheck {
            name: "cargo test".into(),
            kind: CheckKind::Test,
            verdict: VerdictStatus::Passed,
            reproducible: true,
            evidence_hash: None,
        }
    }

    #[test]
    fn for_scope_uses_project_nerve_receipts() {
        let store = ReceiptStore::for_scope(Some(Path::new("/tmp/proj"))).unwrap();
        assert_eq!(store.dir(), Path::new("/tmp/proj/.nerve/receipts"));
    }

    #[test]
    fn issue_seals_persists_and_round_trips_and_verifies() {
        let dir = tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().join("receipts"));
        let signer = LocalEd25519Signer::deterministic_test_key();
        let run = sample_run("add a test");

        let issued = issue_receipt_for_run(
            &run,
            vec![passing_check()],
            Some("toolchain-x".into()),
            Some("policy-1".into()),
            None,
            5000,
            &signer,
            Some(&store),
        )
        .expect("issue persists");

        assert_eq!(issued.run_id, run.run_id);
        assert_eq!(issued.verdict, VerdictStatus::Passed);
        assert_eq!(issued.receipt_id.len(), 64);

        // The persisted receipt reloads, its statement is intact (content address
        // matches), and the local ed25519 signature verifies.
        let loaded = store.load_record(&issued.receipt_id).unwrap();
        assert_eq!(loaded.receipt_id, issued.receipt_id);
        assert_eq!(loaded.schema_version, RECEIPT_SCHEMA_VERSION);
        let verification = nerve_core::receipt::verify_receipt(&loaded, ed25519_verify);
        assert!(verification.statement_intact, "content address holds");
        assert!(verification.signature_valid, "ed25519 signature verifies");
        assert_eq!(verification.verdict, VerdictStatus::Passed);
    }

    #[test]
    fn empty_checks_yield_inconclusive_receipt() {
        let dir = tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().join("receipts"));
        let signer = LocalEd25519Signer::deterministic_test_key();
        let issued = issue_receipt_for_run(
            &sample_run("no checks"),
            vec![],
            None,
            None,
            None,
            1,
            &signer,
            Some(&store),
        )
        .expect("issue persists");
        // INV-R1: no bar exercised => honestly Inconclusive, never a fabricated pass.
        assert_eq!(issued.verdict, VerdictStatus::Inconclusive);
    }

    #[test]
    fn none_store_issues_but_does_not_persist() {
        let signer = LocalEd25519Signer::deterministic_test_key();
        let issued = issue_receipt_for_run(
            &sample_run("ephemeral"),
            vec![passing_check()],
            None,
            None,
            None,
            7,
            &signer,
            None,
        );
        // No store => best-effort returns None (nothing to fetch later), not a panic.
        assert!(issued.is_none());
    }

    #[test]
    fn receipt_get_handler_and_unknown_id() {
        let dir = tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().join("receipts"));
        let signer = LocalEd25519Signer::deterministic_test_key();
        let issued = issue_receipt_for_run(
            &sample_run("fetch me"),
            vec![passing_check()],
            None,
            None,
            None,
            9,
            &signer,
            Some(&store),
        )
        .unwrap();

        let got = run_receipt_get(&issued.receipt_id, Some(&store)).unwrap();
        assert_eq!(got["receipt"]["receipt_id"], json!(issued.receipt_id));
        assert_eq!(got["receipt"]["statement"]["verdict"], json!("passed"));

        // Unknown id and a None store both error (mirror run.get).
        assert!(run_receipt_get("nope", Some(&store)).is_err());
        assert!(run_receipt_get("x", None).is_err());
    }

    #[test]
    fn list_orders_most_recent_first_and_tolerates_missing_dir() {
        let dir = tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().join("receipts"));
        assert!(store.list().unwrap().is_empty(), "missing dir is empty");

        let signer = LocalEd25519Signer::deterministic_test_key();
        // Distinct tasks + distinct issue times => distinct content addresses.
        for (task, ts) in [("a", 100u64), ("b", 300), ("c", 200)] {
            issue_receipt_for_run(
                &sample_run(task),
                vec![passing_check()],
                None,
                None,
                None,
                ts,
                &signer,
                Some(&store),
            )
            .unwrap();
        }
        let order: Vec<u64> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.statement.issued_at_ms)
            .collect();
        assert_eq!(order, vec![300, 200, 100]);
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let raw = json!({
            "schema_version": 999, "receipt_id": "r",
            "statement": {}, "signature": {}
        })
        .to_string();
        let err = deserialize_record(&raw).unwrap_err();
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let dir = tempdir().unwrap();
        let store = ReceiptStore::new(dir.path().to_path_buf());
        let signer = LocalEd25519Signer::deterministic_test_key();
        let stmt =
            nerve_core::receipt::build_statement(&sample_run("x"), vec![], None, None, None, 1);
        let (sig, pk) = signer.sign(&nerve_core::receipt::dsse_pae(
            RECEIPT_PREDICATE_TYPE,
            &nerve_core::receipt::canonical_statement_bytes(&stmt),
        ));
        let mut receipt = nerve_core::receipt::seal_receipt(
            stmt,
            ReceiptSignature {
                payload_type: RECEIPT_PREDICATE_TYPE.to_string(),
                backend: signer.backend().to_string(),
                keyid: signer.keyid(),
                sig,
                public_key: Some(pk),
                bundle: None,
            },
        );
        for bad in ["../escape", "a/b", "", "dots.here"] {
            receipt.receipt_id = bad.to_string();
            assert!(
                store.write_record(&receipt).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }
}
