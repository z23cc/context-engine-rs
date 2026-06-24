//! Durable L1 cross-run **evidence ledger** persistence (`docs/designs/trust-substrate.md`
//! §3 L1, §8) — the append-only transparency log that sits beside the per-run
//! [`RunStore`](crate::run_store). Where L0 captures *one* run as a content-addressed
//! [`Run`](nerve_core::provenance::Run), L1 folds every trust-relevant fact the
//! substrate observes (runs recorded, diffs, policy decisions, verdicts, issued
//! receipts) into a single linear, tamper-evident hash chain so any reader can
//! re-derive it and confirm the log is append-only (INV-R5).
//!
//! ```text
//! .nerve/ledger/log.ndjson    # one JSON LedgerRecord per line, in append order
//! .nerve/ledger/head.json     # the running LedgerHead (count + head_hash)
//! ```
//!
//! Mirrors the verified [`RunStore`](crate::run_store) / [`DelegateStore`](crate::delegate_store)
//! discipline — a versioned record (a `schema_version`, a tolerant load path, and a
//! [`migrate_to_current`] guard owned by THIS module), atomic writes (temp + rename),
//! and **best-effort** appends: a persistence failure NEVER fails the delegated turn
//! ([`append_evidence`] returns `None`). The chaining/canonicalization/SHA-256 it calls
//! lives in `nerve-core::ledger` (INV-R2: hashing is pure + golden-tested); this host
//! module only loads the head, calls [`nerve_core::ledger::append_record`], and persists.

use anyhow::{Context, Result, anyhow};
use nerve_core::ledger::{
    LEDGER_SCHEMA_VERSION, LedgerHead, LedgerKind, LedgerRecord, append_record,
};
use nerve_core::verdict::VerdictStatus;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_FILE: &str = "log.ndjson";
const HEAD_FILE: &str = "head.json";

/// A directory holding the append-only cross-run ledger (`<dir>/log.ndjson` +
/// `<dir>/head.json`). Sibling of [`RunStore`](crate::run_store).
#[derive(Clone)]
pub(crate) struct LedgerStore {
    dir: PathBuf,
}

impl LedgerStore {
    /// Wrap an explicit ledger directory.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Resolve the ledger directory for a scope: `<root>/.nerve/ledger` for a project
    /// root, else the global `config_home()/ledger`.
    pub(crate) fn for_scope(root: Option<&Path>) -> Result<Self> {
        Ok(Self::new(resolve_ledger_dir(root)?))
    }

    /// The backing directory (mirrors `RunStore::dir`; used by tests).
    #[allow(dead_code, reason = "accessor mirroring RunStore::dir; used by tests")]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILE)
    }

    fn head_path(&self) -> PathBuf {
        self.dir.join(HEAD_FILE)
    }

    /// The current chain head: the persisted [`LedgerHead`], or the genesis head when
    /// no head file exists or it is unreadable/stale (tolerant — a missing/corrupt
    /// head degrades to "empty log" rather than blocking an append).
    pub(crate) fn head(&self) -> LedgerHead {
        match fs::read_to_string(self.head_path()) {
            Ok(raw) => deserialize_head(&raw).unwrap_or_else(|_| nerve_core::ledger::empty_head()),
            Err(_) => nerve_core::ledger::empty_head(),
        }
    }

    /// Append one evidence record: load the head, chain `kind` onto it via
    /// [`nerve_core::ledger::append_record`] (host-supplied `appended_at_ms`, never
    /// hashed), append the record as one NDJSON line, then rewrite `head.json`
    /// atomically. Returns the sealed [`LedgerRecord`], or an error if persistence
    /// failed (the best-effort wrapper [`append_evidence`] swallows that into `None`).
    pub(crate) fn append(&self, kind: LedgerKind) -> Result<LedgerRecord> {
        let head = self.head();
        let (record, next_head) = append_record(&head, kind, now_ms());
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create ledger dir {}", self.dir.display()))?;
        let line = serde_json::to_string(&record).context("serialize ledger record")?;
        append_line(&self.log_path(), &line)?;
        let head_json = serde_json::to_string_pretty(&next_head).context("serialize head")?;
        atomic_write(&self.head_path(), head_json.as_bytes())?;
        Ok(record)
    }

    /// All persisted records in append order, tolerating a missing log + bad lines
    /// (a malformed line is skipped, never fatal — mirrors `RunStore::list`).
    pub(crate) fn read_all(&self) -> Vec<LedgerRecord> {
        let raw = match fs::read_to_string(self.log_path()) {
            Ok(raw) => raw,
            Err(_) => return Vec::new(),
        };
        raw.lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| deserialize_record(line).ok())
            .collect()
    }
}

/// Best-effort cross-run append: chain `kind` onto the ledger and persist. A `None`
/// store (no served root) or any persistence failure yields `None` — provenance is an
/// audit seam, never a gate on the delegated turn (INV-R2). The caller announces a
/// successful append via [`RuntimeEvent::ledger_appended`](nerve_runtime::RuntimeEvent).
pub(crate) fn append_evidence(
    store: Option<&LedgerStore>,
    kind: LedgerKind,
) -> Option<LedgerRecord> {
    store?.append(kind).ok()
}

/// Resolve a `ledger.query`: filter [`LedgerStore::read_all`] by the optional facets
/// and return `{"records":[...]}` (newest-matching first, capped at `limit`). A `None`
/// store yields an empty list. The `outcome` facet matches a [`VerdictStatus`] against
/// `Verdict`/`ReceiptIssued` records; `record_kind` matches the serde `kind` tag.
pub(crate) fn run_ledger_query(
    store: Option<&LedgerStore>,
    run_id: Option<&str>,
    agent: Option<&str>,
    diff_hash: Option<&str>,
    outcome: Option<VerdictStatus>,
    record_kind: Option<&str>,
    limit: u64,
) -> Value {
    let all = store.map(LedgerStore::read_all).unwrap_or_default();
    let records: Vec<Value> = all
        .into_iter()
        .rev()
        .filter(|r| matches_filters(r, run_id, agent, diff_hash, outcome, record_kind))
        .take(usize::try_from(limit).unwrap_or(usize::MAX))
        .map(|r| serde_json::to_value(&r).unwrap_or(Value::Null))
        .collect();
    json!({ "records": records })
}

/// Whether one record passes every supplied facet (an unset facet always matches).
fn matches_filters(
    record: &LedgerRecord,
    run_id: Option<&str>,
    agent: Option<&str>,
    diff_hash: Option<&str>,
    outcome: Option<VerdictStatus>,
    record_kind: Option<&str>,
) -> bool {
    if let Some(want) = run_id
        && kind_run_id(&record.kind) != Some(want)
    {
        return false;
    }
    if let Some(want) = agent
        && kind_agent(&record.kind) != Some(want)
    {
        return false;
    }
    if let Some(want) = diff_hash
        && kind_diff_hash(&record.kind) != Some(want)
    {
        return false;
    }
    if let Some(want) = outcome
        && kind_verdict(&record.kind) != Some(want)
    {
        return false;
    }
    if let Some(want) = record_kind
        && kind_tag(&record.kind) != want
    {
        return false;
    }
    true
}

/// The serde `kind` tag for a [`LedgerKind`] (matches the on-wire discriminant).
fn kind_tag(kind: &LedgerKind) -> &'static str {
    match kind {
        LedgerKind::RunRecorded { .. } => "run_recorded",
        LedgerKind::DiffRecorded { .. } => "diff_recorded",
        LedgerKind::PolicyDecision { .. } => "policy_decision",
        LedgerKind::Verdict { .. } => "verdict",
        LedgerKind::ReceiptIssued { .. } => "receipt_issued",
    }
}

/// The `run_id` a record is about (every current kind carries one).
fn kind_run_id(kind: &LedgerKind) -> Option<&str> {
    match kind {
        LedgerKind::RunRecorded { run_id, .. }
        | LedgerKind::DiffRecorded { run_id, .. }
        | LedgerKind::PolicyDecision { run_id, .. }
        | LedgerKind::Verdict { run_id, .. }
        | LedgerKind::ReceiptIssued { run_id, .. } => Some(run_id.as_str()),
    }
}

/// The agent a record names, when the kind carries one (`RunRecorded` only today).
fn kind_agent(kind: &LedgerKind) -> Option<&str> {
    match kind {
        LedgerKind::RunRecorded { agent, .. } => Some(agent.as_str()),
        _ => None,
    }
}

/// The diff hash a record names, when present (`DiffRecorded`, or a `Verdict` whose
/// optional `diff_hash` is set).
fn kind_diff_hash(kind: &LedgerKind) -> Option<&str> {
    match kind {
        LedgerKind::DiffRecorded { diff_hash, .. } => Some(diff_hash.as_str()),
        LedgerKind::Verdict { diff_hash, .. } => diff_hash.as_deref(),
        _ => None,
    }
}

/// The load-bearing [`VerdictStatus`] a record carries, when any (`Verdict` /
/// `ReceiptIssued`).
fn kind_verdict(kind: &LedgerKind) -> Option<VerdictStatus> {
    match kind {
        LedgerKind::Verdict { verdict, .. } | LedgerKind::ReceiptIssued { verdict, .. } => {
            Some(*verdict)
        }
        _ => None,
    }
}

/// Append one NDJSON line to `path`, creating the file on demand. Unlike the per-id
/// stores this is an O_APPEND write (the log grows by one line); the head rewrite that
/// follows is the atomic step a reader keys off of.
fn append_line(path: &Path, line: &str) -> Result<()> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("failed to append to {}", path.display()))
}

/// Atomic file write: temp file + rename, so a reader never observes a half-written
/// head (mirrors `RunStore::atomic_write`).
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("ledger-write")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Parse + migrate one record line, tolerant of an older/missing `schema_version`
/// (treated as v1); rejects a newer-than-supported version.
fn deserialize_record(raw: &str) -> Result<LedgerRecord> {
    let mut value: Value = serde_json::from_str(raw).context("invalid ledger record JSON")?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    migrate_to_current(&mut value, version)?;
    serde_json::from_value(value).context("ledger record shape mismatch")
}

/// Parse + migrate the head, with the same version tolerance as a record.
fn deserialize_head(raw: &str) -> Result<LedgerHead> {
    let mut value: Value = serde_json::from_str(raw).context("invalid ledger head JSON")?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    migrate_to_current(&mut value, version)?;
    serde_json::from_value(value).context("ledger head shape mismatch")
}

/// Upgrade a ledger `value` from `version` to [`LEDGER_SCHEMA_VERSION`] in place. Only
/// one version exists today, so this is the newer-than-known guard + a re-stamp; add an
/// arm per future bump (mirrors `RunStore::migrate_to_current`).
fn migrate_to_current(value: &mut Value, version: u64) -> Result<()> {
    if version > u64::from(LEDGER_SCHEMA_VERSION) {
        return Err(anyhow!(
            "ledger schema_version {version} is newer than supported {LEDGER_SCHEMA_VERSION}; upgrade nerve"
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("schema_version".into(), Value::from(LEDGER_SCHEMA_VERSION));
    }
    Ok(())
}

/// `<root>/.nerve/ledger` for a project root, else the global `config_home()/ledger`.
fn resolve_ledger_dir(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root.join(".nerve").join("ledger")),
        None => {
            let home = nerve_agent::auth::config_home().map_err(|err| anyhow!("{err}"))?;
            Ok(home.join("ledger"))
        }
    }
}

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
    use nerve_core::ledger::verify_chain;
    use tempfile::tempdir;

    fn run_recorded(n: u64) -> LedgerKind {
        LedgerKind::RunRecorded {
            run_id: format!("run-{n}"),
            run_root_hash: format!("root-{n}"),
            agent: if n.is_multiple_of(2) {
                "codex"
            } else {
                "claude"
            }
            .into(),
            task_hash: format!("task-{n}"),
            event_count: n,
        }
    }

    #[test]
    fn for_scope_uses_project_nerve_ledger() {
        let store = LedgerStore::for_scope(Some(Path::new("/tmp/proj"))).unwrap();
        assert_eq!(store.dir(), Path::new("/tmp/proj/.nerve/ledger"));
    }

    #[test]
    fn append_chains_persists_and_verifies() {
        let dir = tempdir().unwrap();
        let store = LedgerStore::new(dir.path().join("ledger"));

        // Empty store: genesis head, empty read.
        assert_eq!(store.head().count, 0);
        assert!(store.read_all().is_empty(), "missing log is empty");

        let r0 = store.append(run_recorded(0)).unwrap();
        let r1 = store.append(run_recorded(1)).unwrap();
        assert_eq!(r0.seq, 0);
        assert_eq!(r1.seq, 1);
        assert_eq!(r1.prev_hash, r0.record_hash);

        // Head advanced and points at the last record.
        let head = store.head();
        assert_eq!(head.count, 2);
        assert_eq!(head.head_hash, r1.record_hash);

        // read_all is in append order and re-derives a consistent chain.
        let all = store.read_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].record_hash, r0.record_hash);
        let verified = verify_chain(&all).expect("chain verifies");
        assert_eq!(verified.head_hash, head.head_hash);
        assert_eq!(verified.count, 2);
    }

    #[test]
    fn append_evidence_is_best_effort() {
        let dir = tempdir().unwrap();
        let store = LedgerStore::new(dir.path().join("ledger"));
        assert!(append_evidence(Some(&store), run_recorded(0)).is_some());
        // No store -> None, never a panic.
        assert!(append_evidence(None, run_recorded(0)).is_none());
    }

    #[test]
    fn query_filters_by_run_agent_diff_outcome_and_kind() {
        let dir = tempdir().unwrap();
        let store = LedgerStore::new(dir.path().join("ledger"));
        store.append(run_recorded(0)).unwrap(); // run-0, codex, run_recorded
        store.append(run_recorded(1)).unwrap(); // run-1, claude, run_recorded
        store
            .append(LedgerKind::DiffRecorded {
                run_id: "run-0".into(),
                diff_hash: "deadbeef".into(),
                files: 1,
                added: 2,
                removed: 0,
            })
            .unwrap();
        store
            .append(LedgerKind::Verdict {
                run_id: "run-0".into(),
                diff_hash: Some("deadbeef".into()),
                verdict: VerdictStatus::Passed,
                checks: vec![],
                advisory_llm_judge: None,
            })
            .unwrap();

        // by run_id
        let by_run = run_ledger_query(Some(&store), Some("run-0"), None, None, None, None, 200);
        assert_eq!(by_run["records"].as_array().unwrap().len(), 3);

        // by agent (only RunRecorded carry one)
        let by_agent = run_ledger_query(Some(&store), None, Some("claude"), None, None, None, 200);
        let agent_recs = by_agent["records"].as_array().unwrap();
        assert_eq!(agent_recs.len(), 1);
        assert_eq!(agent_recs[0]["kind"]["run_id"], json!("run-1"));

        // by diff_hash (DiffRecorded + Verdict)
        let by_diff = run_ledger_query(Some(&store), None, None, Some("deadbeef"), None, None, 200);
        assert_eq!(by_diff["records"].as_array().unwrap().len(), 2);

        // by outcome (Verdict)
        let by_outcome = run_ledger_query(
            Some(&store),
            None,
            None,
            None,
            Some(VerdictStatus::Passed),
            None,
            200,
        );
        assert_eq!(by_outcome["records"].as_array().unwrap().len(), 1);

        // by record_kind tag
        let by_kind = run_ledger_query(
            Some(&store),
            None,
            None,
            None,
            None,
            Some("run_recorded"),
            200,
        );
        assert_eq!(by_kind["records"].as_array().unwrap().len(), 2);

        // newest-matching first + limit
        let limited = run_ledger_query(Some(&store), None, None, None, None, None, 1);
        let limited_recs = limited["records"].as_array().unwrap();
        assert_eq!(limited_recs.len(), 1);
        assert_eq!(limited_recs[0]["kind"]["kind"], json!("verdict"));
    }

    #[test]
    fn query_none_store_is_empty() {
        let result = run_ledger_query(None, None, None, None, None, None, 200);
        assert_eq!(result["records"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn read_all_tolerates_bad_lines() {
        let dir = tempdir().unwrap();
        let store = LedgerStore::new(dir.path().join("ledger"));
        store.append(run_recorded(0)).unwrap();
        // Inject a garbage line + a blank line into the log; read_all skips them.
        append_line(&store.log_path(), "not json at all").unwrap();
        append_line(&store.log_path(), "").unwrap();
        store.append(run_recorded(1)).unwrap();
        let all = store.read_all();
        assert_eq!(all.len(), 2, "garbage + blank lines skipped");
        assert_eq!(all[0].seq, 0);
        assert_eq!(all[1].seq, 1);
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let raw = json!({
            "schema_version": 999, "seq": 0,
            "kind": {"kind": "diff_recorded", "run_id": "r", "diff_hash": "d",
                     "files": 1, "added": 0, "removed": 0},
            "record_hash": "aa", "prev_hash": ""
        })
        .to_string();
        let err = deserialize_record(&raw).unwrap_err();
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }

    #[test]
    fn corrupt_head_degrades_to_empty_then_append_still_chains() {
        let dir = tempdir().unwrap();
        let store = LedgerStore::new(dir.path().join("ledger"));
        fs::create_dir_all(store.dir()).unwrap();
        fs::write(store.head_path(), b"{ not valid json").unwrap();
        // Corrupt head reads as genesis, so an append starts a fresh chain at seq 0.
        assert_eq!(store.head().count, 0);
        let r0 = store.append(run_recorded(0)).unwrap();
        assert_eq!(r0.seq, 0);
        assert_eq!(r0.prev_hash, "");
    }
}
