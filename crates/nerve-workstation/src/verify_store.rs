//! Durable L2 **verdict** persistence (`docs/designs/trust-substrate.md` §3 L2) —
//! the sibling of [`RunStore`](crate::run_store). Every execution-grounded
//! re-verification of a captured [`Run`](nerve_core::provenance::Run) is sealed by
//! [`nerve_core::verdict::build_verdict`] into a content-addressed
//! [`Verdict`](nerve_core::verdict::Verdict) and persisted so it can be fetched
//! (`verify.get`) and enumerated (`verify.list`).
//!
//! ```text
//! .nerve/verdicts/<verdict_id>.json   # the versioned Verdict (id == content address)
//! ```
//!
//! Mirrors the verified [`RunStore`] discipline — a versioned record (a
//! `schema_version`, a tolerant [`load_record`](VerifyStore::load_record) path, and
//! a [`migrate_to_current`] seam owned by THIS module), atomic writes (temp +
//! rename), and **best-effort** persistence: a write failure NEVER fails the
//! verify turn (the verdict is still returned to the caller). Persistence lives here
//! in `nerve-workstation`, above the determinism boundary; the pure
//! canonicalization + hashing it commits to lives in `nerve-core::verdict` (INV-R2).

use anyhow::{Context, Result, anyhow};
use nerve_core::verdict::{VERDICT_SCHEMA_VERSION, Verdict};
use nerve_runtime::RuntimeError;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// A directory of persisted verdicts (`<dir>/<verdict_id>.json`). Sibling of
/// [`RunStore`](crate::run_store).
#[derive(Clone)]
pub(crate) struct VerifyStore {
    dir: PathBuf,
}

impl VerifyStore {
    /// Wrap an explicit verdicts directory.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Resolve the verdicts directory for a scope: `<root>/.nerve/verdicts` for a
    /// project root, else the global `config_home()/verdicts`.
    pub(crate) fn for_scope(root: Option<&Path>) -> Result<Self> {
        Ok(Self::new(resolve_verdicts_dir(root)?))
    }

    /// The backing directory (mirrors `RunStore::dir`; used by tests).
    #[allow(dead_code, reason = "accessor mirroring RunStore::dir; used by tests")]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The per-verdict file `<dir>/<verdict_id>.json` (validating the id stays in-dir).
    fn path_for(&self, verdict_id: &str) -> Result<PathBuf> {
        validate_id(verdict_id)?;
        Ok(self.dir.join(format!("{verdict_id}.json")))
    }

    /// Persist a verdict atomically (temp + rename), creating the dir on demand.
    pub(crate) fn write_record(&self, verdict: &Verdict) -> Result<()> {
        let path = self.path_for(&verdict.verdict_id)?;
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create verdicts dir {}", self.dir.display()))?;
        let json = serde_json::to_string_pretty(verdict).context("serialize verdict")?;
        atomic_write(&path, json.as_bytes())
    }

    /// Load and migrate one verdict by id.
    pub(crate) fn load_record(&self, verdict_id: &str) -> Result<Verdict> {
        let path = self.path_for(verdict_id)?;
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        deserialize_record(&raw).with_context(|| format!("failed to parse verdict {verdict_id}"))
    }

    /// All persisted verdicts, most recent first (tolerating a missing dir + bad
    /// files). Ordered by `verified_at_ms` desc, then `verdict_id` for stability.
    pub(crate) fn list(&self) -> Result<Vec<Verdict>> {
        let mut verdicts = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(verdicts),
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
            if let Ok(verdict) = deserialize_record(&raw) {
                verdicts.push(verdict);
            }
        }
        verdicts.sort_by(|a, b| {
            b.verified_at_ms
                .cmp(&a.verified_at_ms)
                .then_with(|| b.verdict_id.cmp(&a.verdict_id))
        });
        Ok(verdicts)
    }
}

/// Resolve a `verify.get`: the full sealed [`Verdict`] by id. An unknown id (or no
/// served root) is an error, mirroring `run.get`.
pub(crate) fn run_verify_get(
    verdict_id: &str,
    store: Option<&VerifyStore>,
) -> Result<Value, RuntimeError> {
    let store = store.ok_or_else(|| RuntimeError::adapter(format!("no verdict `{verdict_id}`")))?;
    let verdict = store
        .load_record(verdict_id)
        .map_err(|err| RuntimeError::adapter(format!("no verdict `{verdict_id}`: {err}")))?;
    let verdict = serde_json::to_value(&verdict).map_err(|err| {
        RuntimeError::adapter(format!("failed to render verdict `{verdict_id}`: {err}"))
    })?;
    Ok(serde_json::json!({ "verdict": verdict }))
}

/// Resolve a `verify.list`: all sealed verdicts for the served scope, newest first,
/// optionally filtered to one `run_id`. `None` store (no served root) yields empty.
pub(crate) fn run_verify_list(store: Option<&VerifyStore>, run_id: Option<&str>) -> Value {
    let verdicts = store
        .and_then(|s| s.list().ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|v| run_id.is_none_or(|id| v.run_id == id))
        .map(|v| serde_json::to_value(&v).unwrap_or(Value::Null))
        .collect::<Vec<_>>();
    serde_json::json!({ "verdicts": verdicts })
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
            .unwrap_or("verdict-write")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Parse + migrate a verdict, tolerant of an older/missing `schema_version` (treated
/// as v1); rejects a newer-than-supported version.
fn deserialize_record(raw: &str) -> Result<Verdict> {
    let mut value: Value = serde_json::from_str(raw).context("invalid verdict JSON")?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    migrate_to_current(&mut value, version)?;
    serde_json::from_value(value).context("verdict shape mismatch")
}

/// Upgrade a verdict `value` from `version` to [`VERDICT_SCHEMA_VERSION`] in place.
/// Only one version exists today, so this is the newer-than-known guard + a re-stamp;
/// add an arm per future bump (mirrors `RunStore::migrate_to_current`).
fn migrate_to_current(value: &mut Value, version: u64) -> Result<()> {
    if version > u64::from(VERDICT_SCHEMA_VERSION) {
        return Err(anyhow!(
            "verdict schema_version {version} is newer than supported {VERDICT_SCHEMA_VERSION}; upgrade nerve"
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("schema_version".into(), Value::from(VERDICT_SCHEMA_VERSION));
    }
    Ok(())
}

/// `<root>/.nerve/verdicts` for a project root, else the global `config_home()/verdicts`.
fn resolve_verdicts_dir(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root.join(".nerve").join("verdicts")),
        None => {
            let home = nerve_agent::auth::config_home().map_err(|err| anyhow!("{err}"))?;
            Ok(home.join("verdicts"))
        }
    }
}

/// Reject ids that could escape the verdicts directory (same token rule as the other
/// stores). A content-address verdict id is hex, so it always passes; this guards a
/// malformed/empty id from reaching the filesystem.
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
            "invalid verdict id '{id}': use only letters, digits, '-' and '_'"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::verdict::{CheckKind, CheckResult, CheckStatus, VerdictStatus, build_verdict};
    use serde_json::json;
    use tempfile::tempdir;

    fn check(name: &str, status: CheckStatus) -> CheckResult {
        CheckResult {
            name: name.into(),
            kind: CheckKind::Test,
            status,
            reproducible: true,
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 5,
            output_hash: String::new(),
            runs: 1,
            passed: 1,
        }
    }

    fn sample_verdict(run_id: &str, verified_at_ms: u64) -> Verdict {
        build_verdict(
            run_id,
            None,
            "spec-1",
            "closure-1",
            vec![check("cargo test", CheckStatus::Pass)],
            &[true],
            verified_at_ms,
        )
    }

    #[test]
    fn for_scope_uses_project_nerve_verdicts() {
        let store = VerifyStore::for_scope(Some(Path::new("/tmp/proj"))).unwrap();
        assert_eq!(store.dir(), Path::new("/tmp/proj/.nerve/verdicts"));
    }

    #[test]
    fn write_load_round_trips_and_preserves_content_id() {
        let dir = tempdir().unwrap();
        let store = VerifyStore::new(dir.path().join("verdicts"));
        let verdict = sample_verdict("run-1", 1000);
        assert_eq!(verdict.verdict_id.len(), 64);
        assert_eq!(verdict.status, VerdictStatus::Passed);

        store.write_record(&verdict).unwrap();
        let loaded = store.load_record(&verdict.verdict_id).unwrap();
        assert_eq!(loaded.verdict_id, verdict.verdict_id);
        assert_eq!(loaded.schema_version, VERDICT_SCHEMA_VERSION);
        assert_eq!(loaded.status, VerdictStatus::Passed);
        assert_eq!(loaded.checks.len(), 1);
    }

    #[test]
    fn list_orders_recent_first_filters_by_run_and_tolerates_missing_dir() {
        let dir = tempdir().unwrap();
        let store = VerifyStore::new(dir.path().join("verdicts"));
        assert!(store.list().unwrap().is_empty(), "missing dir is empty");

        // Distinct run_ids -> distinct content addresses -> distinct files.
        store.write_record(&sample_verdict("run-a", 100)).unwrap();
        store.write_record(&sample_verdict("run-b", 300)).unwrap();
        store.write_record(&sample_verdict("run-c", 200)).unwrap();

        let order: Vec<u64> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|v| v.verified_at_ms)
            .collect();
        assert_eq!(order, vec![300, 200, 100]);

        // run.list filter narrows to one run's verdicts.
        let listed = run_verify_list(Some(&store), Some("run-b"));
        let arr = listed["verdicts"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["run_id"], json!("run-b"));
    }

    #[test]
    fn get_and_list_handlers_and_none_store() {
        let dir = tempdir().unwrap();
        let store = VerifyStore::new(dir.path().join("verdicts"));
        let verdict = sample_verdict("run-1", 1000);
        store.write_record(&verdict).unwrap();

        let got = run_verify_get(&verdict.verdict_id, Some(&store)).unwrap();
        assert_eq!(got["verdict"]["verdict_id"], json!(verdict.verdict_id));

        assert!(run_verify_get("nope", Some(&store)).is_err());
        assert!(run_verify_get("x", None).is_err());
        assert_eq!(
            run_verify_list(None, None)["verdicts"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let raw = json!({
            "schema_version": 999, "verdict_id": "v", "run_id": "r",
            "status": "passed", "checkspec_hash": "h", "checks": []
        })
        .to_string();
        let err = deserialize_record(&raw).unwrap_err();
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let dir = tempdir().unwrap();
        let store = VerifyStore::new(dir.path().to_path_buf());
        for bad in ["../escape", "a/b", "", "dots.here"] {
            let mut verdict = sample_verdict("run-1", 1);
            verdict.verdict_id = bad.to_string();
            assert!(
                store.write_record(&verdict).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }
}
