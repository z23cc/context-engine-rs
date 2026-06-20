//! P5 — durable flow persistence (Wave C4), the sibling of [`SessionStore`].
//!
//! North-star §5 keeps live daemon **jobs** in-memory by design, but an hours-long
//! fleet run must survive a daemon restart. The orchestration design (§5) resolves
//! this by persisting the **ledger, not the live threads**: a [`FlowStore`] records,
//! per flow, the [`WorkflowDef`] + the append-only [`WorkerLedger`] tape (+ an
//! `artifacts/` dir for node-produced diffs/files), so a finished flow is
//! INSPECTABLE and a recorded one is REPLAYABLE (`flow.replay`) after the daemon
//! exits.
//!
//! ```text
//! .nerve/flows/<flow_id>/
//!   def.json          # the WorkflowDef (the strategy as data)
//!   ledger.jsonl      # the append-only WorkerLedger (tape = blackboard = record)
//!   record.json       # the versioned FlowRecord (status + outcome + metadata)
//!   artifacts/        # diffs / files produced by nodes (reserved; created lazily)
//! ```
//!
//! Mirrors the verified versioned [`SessionStore`] discipline: a [`FlowRecord`] with
//! `schema_version` + a tolerant [`load`](FlowStore::load) path + a
//! [`migrate_to_current`] seam, owned by THIS module so the on-disk schema evolves
//! independently of the protocol/domain types. Writes are **atomic** (temp file +
//! rename) and happen at **node boundaries** (the engine's natural checkpoint), so a
//! crash mid-node never leaves a torn record. Promote `ledger.jsonl` → SQLite only on
//! a measured trigger (north-star invariant 8).
//!
//! [`SessionStore`]: crate::session::SessionStore

use crate::worker::WorkerLedger;
use anyhow::{Context, Result, anyhow};
use nerve_runtime::{Strategy, WorkflowDef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current on-disk flow-record schema. Bump when [`FlowRecord`] changes shape, and
/// add a migration arm in [`migrate_to_current`] for the previous version.
const SCHEMA_VERSION: u32 = 1;

/// A persisted flow's metadata record (the live tape lives beside it in
/// `ledger.jsonl`; the def in `def.json`). Versioned exactly like
/// [`SessionRecord`](crate::session::SessionRecord).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FlowRecord {
    /// On-disk schema version, for migration. See [`SCHEMA_VERSION`].
    pub(crate) schema_version: u32,
    /// The flow id (also the directory name and the `flow.start` job id).
    pub(crate) flow_id: String,
    /// The workflow's human-readable name.
    pub(crate) name: String,
    /// A stable strategy label (`single` / `parallel` / `pipeline` / …).
    pub(crate) strategy: String,
    /// Unix-epoch milliseconds when the flow started.
    pub(crate) started_at_ms: u64,
    /// Unix-epoch milliseconds of the last node-boundary persist (None until first).
    #[serde(default)]
    pub(crate) updated_at_ms: Option<u64>,
    /// Whether the flow has reached a terminal state.
    #[serde(default)]
    pub(crate) finished: bool,
    /// The terminal outcome, present once the flow finished.
    #[serde(default)]
    pub(crate) outcome: Option<FlowStoreOutcome>,
}

/// The persisted terminal outcome (self-owned mirror, so the on-disk schema is
/// independent of the protocol's [`FlowRunOutcome`](nerve_runtime::FlowRunOutcome)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FlowStoreOutcome {
    pub(crate) ok: bool,
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) final_text: String,
}

impl FlowRecord {
    /// Begin a fresh record for a starting flow.
    pub(crate) fn begin(flow_id: &str, def: &WorkflowDef) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            flow_id: flow_id.to_string(),
            name: def.name.clone(),
            strategy: strategy_label(&def.strategy).to_string(),
            started_at_ms: now_ms(),
            updated_at_ms: None,
            finished: false,
            outcome: None,
        }
    }

    /// Stamp the terminal outcome.
    pub(crate) fn finish(&mut self, ok: bool, summary: String, final_text: String) {
        self.finished = true;
        self.updated_at_ms = Some(now_ms());
        self.outcome = Some(FlowStoreOutcome {
            ok,
            summary,
            final_text,
        });
    }
}

/// A directory of persisted flows (`<dir>/<flow_id>/...`). Sibling of
/// [`SessionStore`](crate::session::SessionStore).
#[derive(Clone)]
pub(crate) struct FlowStore {
    dir: PathBuf,
}

impl FlowStore {
    /// Wrap an explicit flows directory.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Resolve the flows directory for a scope: `<root>/.nerve/flows` for a project
    /// root, else the global `config_home()/flows` when no root is known.
    pub(crate) fn for_scope(root: Option<&Path>) -> Result<Self> {
        Ok(Self::new(resolve_flows_dir(root)?))
    }

    /// The backing directory (mirrors `SessionStore::dir`; used by tests + future
    /// inspection tooling).
    #[allow(
        dead_code,
        reason = "accessor mirroring SessionStore::dir; used by tests"
    )]
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// The per-flow directory `<dir>/<flow_id>` (validating the id stays in-dir).
    fn flow_dir(&self, flow_id: &str) -> Result<PathBuf> {
        validate_id(flow_id)?;
        Ok(self.dir.join(flow_id))
    }

    /// Persist a flow's `def.json` (the [`WorkflowDef`]) at `flow.start`. Idempotent:
    /// writing the same def twice is harmless. Creates the per-flow directory.
    pub(crate) fn write_def(&self, flow_id: &str, def: &WorkflowDef) -> Result<()> {
        let dir = self.flow_dir(flow_id)?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create flow dir {}", dir.display()))?;
        let json = serde_json::to_string_pretty(def).context("serialize workflow def")?;
        atomic_write(&dir.join("def.json"), json.as_bytes())
    }

    /// Persist the metadata record (the versioned [`FlowRecord`]) atomically.
    pub(crate) fn write_record(&self, record: &FlowRecord) -> Result<()> {
        let dir = self.flow_dir(&record.flow_id)?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create flow dir {}", dir.display()))?;
        let json = serde_json::to_string_pretty(record).context("serialize flow record")?;
        atomic_write(&dir.join("record.json"), json.as_bytes())
    }

    /// Persist the current ledger tape to `ledger.jsonl` ATOMICALLY. Called at NODE
    /// BOUNDARIES (the engine's checkpoint): the ledger is append-only, so re-writing
    /// the whole (small) file via temp+rename is torn-write-safe and simpler than an
    /// append + fsync dance. Promote to true append / SQLite only on a measured
    /// trigger (north-star invariant 8).
    pub(crate) fn write_ledger(&self, flow_id: &str, ledger: &WorkerLedger) -> Result<()> {
        let dir = self.flow_dir(flow_id)?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create flow dir {}", dir.display()))?;
        atomic_write(&dir.join("ledger.jsonl"), ledger.to_jsonl().as_bytes())
    }

    /// Load and migrate a flow's [`FlowRecord`].
    pub(crate) fn load_record(&self, flow_id: &str) -> Result<FlowRecord> {
        let path = self.flow_dir(flow_id)?.join("record.json");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        deserialize_record(&raw).with_context(|| format!("failed to parse flow {flow_id}"))
    }

    /// Load a flow's persisted [`WorkflowDef`].
    pub(crate) fn load_def(&self, flow_id: &str) -> Result<WorkflowDef> {
        let path = self.flow_dir(flow_id)?.join("def.json");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("failed to parse def for {flow_id}"))
    }

    /// Load a flow's recorded ledger tape (the replay source for `flow.replay` +
    /// resume). Reconstructs the [`WorkerLedger`] from `ledger.jsonl`.
    pub(crate) fn load_ledger(&self, flow_id: &str) -> Result<WorkerLedger> {
        let path = self.flow_dir(flow_id)?.join("ledger.jsonl");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        WorkerLedger::from_jsonl(&raw)
            .with_context(|| format!("failed to parse ledger for {flow_id}"))
    }

    /// The `artifacts/` directory for a flow (node-produced diffs/files), created on
    /// demand. Reserved seam (design §5): the engine does not yet write artifacts, so
    /// this is dead until a node-artifact producer lands (a later wave).
    #[allow(
        dead_code,
        reason = "reserved artifacts seam (design §5); first producer lands in a later wave"
    )]
    pub(crate) fn artifacts_dir(&self, flow_id: &str) -> Result<PathBuf> {
        let dir = self.flow_dir(flow_id)?.join("artifacts");
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create artifacts dir {}", dir.display()))?;
        Ok(dir)
    }

    /// All persisted flow records, most recent first (for inspection / a future
    /// `flow.list` that includes finished flows from disk).
    pub(crate) fn list(&self) -> Result<Vec<FlowRecord>> {
        let mut records = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(err) => return Err(anyhow!("failed to read {}: {err}", self.dir.display())),
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let record_path = entry.path().join("record.json");
            let Ok(raw) = fs::read_to_string(&record_path) else {
                continue;
            };
            if let Ok(record) = deserialize_record(&raw) {
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            b.started_at_ms
                .cmp(&a.started_at_ms)
                .then_with(|| b.flow_id.cmp(&a.flow_id))
        });
        Ok(records)
    }
}

/// Atomic file write: write to a sibling temp file, then rename over the target, so a
/// reader never observes a half-written file (the persistence discipline §5 calls
/// for). `rename` is atomic within a directory on the platforms we target.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("path {} has no parent", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("flow-write")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Parse + migrate a flow record, tolerant of an older/missing `schema_version`
/// (treated as v1, defaults filling gaps); rejects a newer-than-supported version.
fn deserialize_record(raw: &str) -> Result<FlowRecord> {
    let mut value: Value = serde_json::from_str(raw).context("invalid flow JSON")?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    migrate_to_current(&mut value, version)?;
    serde_json::from_value(value).context("flow record shape mismatch")
}

/// Upgrade a flow record `value` from `version` to [`SCHEMA_VERSION`] in place. Only
/// one version exists today, so this is the newer-than-known guard + a version
/// re-stamp; add an arm per future bump, oldest-first (mirrors `SessionStore`).
fn migrate_to_current(value: &mut Value, version: u64) -> Result<()> {
    if version > u64::from(SCHEMA_VERSION) {
        return Err(anyhow!(
            "flow schema_version {version} is newer than supported {SCHEMA_VERSION}; upgrade nerve"
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
    }
    Ok(())
}

/// `<root>/.nerve/flows` for a project root, else the global `config_home()/flows`.
fn resolve_flows_dir(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root.join(".nerve").join("flows")),
        None => {
            let home = nerve_agent::auth::config_home().map_err(|err| anyhow!("{err}"))?;
            Ok(home.join("flows"))
        }
    }
}

/// Reject ids that could escape the flows directory (same token rule as
/// `SessionStore`: ASCII alphanumerics plus `-`/`_`, so `<id>` stays in-dir).
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
            "invalid flow id '{id}': use only letters, digits, '-' and '_'"
        ))
    }
}

/// A stable label for a strategy (mirrors the flow-job projection).
fn strategy_label(strategy: &Strategy) -> &'static str {
    match strategy {
        Strategy::Single { .. } => "single",
        Strategy::Parallel { .. } => "parallel",
        Strategy::Pipeline { .. } => "pipeline",
        Strategy::MapReduce { .. } => "map_reduce",
        Strategy::VoteJudge { .. } => "vote_judge",
        Strategy::Debate { .. } => "debate",
        Strategy::Hierarchical { .. } => "hierarchical",
        _ => "unknown",
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
    use nerve_runtime::{DelegateAutonomy, FailPolicy, Step, TaskTemplate, WorkerRef};
    use tempfile::tempdir;

    fn single_def(name: &str) -> WorkflowDef {
        WorkflowDef {
            schema_version: 1,
            name: name.into(),
            strategy: Strategy::Single {
                step: Step {
                    worker: WorkerRef::Cli {
                        name: "claude".into(),
                    },
                    task: TaskTemplate::new("do it"),
                    autonomy: DelegateAutonomy::ReadOnly,
                    on_fail: FailPolicy::Abort,
                },
            },
            budget: nerve_runtime::BudgetSpec::default(),
            max_depth: 2,
        }
    }

    fn ledger_with_one_node() -> WorkerLedger {
        let ledger = WorkerLedger::new();
        ledger.record_start("node-0", "do it", 7);
        ledger.record_event(
            "node-0",
            crate::worker::WorkerEvent::Step(nerve_runtime::AgentEventKind::Message {
                text: "answer".into(),
            }),
        );
        ledger.record_result(
            "node-0",
            &crate::worker::TurnResult {
                ok: true,
                text: "answer".into(),
                usage: nerve_agent::Usage::default(),
                cost_usd: None,
                timed_out: false,
            },
        );
        ledger
    }

    #[test]
    fn for_scope_uses_project_nerve_flows() {
        let store = FlowStore::for_scope(Some(Path::new("/tmp/proj"))).unwrap();
        assert_eq!(store.dir(), Path::new("/tmp/proj/.nerve/flows"));
    }

    #[test]
    fn def_record_and_ledger_round_trip() {
        let dir = tempdir().unwrap();
        let store = FlowStore::new(dir.path().join("flows"));
        let def = single_def("triage");
        let mut record = FlowRecord::begin("job-1", &def);

        store.write_def("job-1", &def).unwrap();
        store.write_record(&record).unwrap();
        let ledger = ledger_with_one_node();
        store.write_ledger("job-1", &ledger).unwrap();

        // def + ledger round-trip.
        let loaded_def = store.load_def("job-1").unwrap();
        assert_eq!(loaded_def, def);
        let loaded_ledger = store.load_ledger("job-1").unwrap();
        assert_eq!(loaded_ledger.to_jsonl(), ledger.to_jsonl());
        assert_eq!(loaded_ledger.output("node-0"), Some("answer".to_string()));

        // record round-trips, and a finish stamps the outcome.
        record.finish(true, "single: ok".into(), "answer".into());
        store.write_record(&record).unwrap();
        let loaded = store.load_record("job-1").unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.flow_id, "job-1");
        assert_eq!(loaded.strategy, "single");
        assert!(loaded.finished);
        assert_eq!(loaded.outcome.unwrap().final_text, "answer");
    }

    #[test]
    fn list_orders_most_recent_first_and_tolerates_missing_dir() {
        let dir = tempdir().unwrap();
        let store = FlowStore::new(dir.path().join("flows"));
        assert!(store.list().unwrap().is_empty(), "missing dir is empty");

        let def = single_def("a");
        for (id, ts) in [("flow-1", 100u64), ("flow-2", 300), ("flow-3", 200)] {
            let mut record = FlowRecord::begin(id, &def);
            record.started_at_ms = ts;
            store.write_record(&record).unwrap();
        }
        let order: Vec<u64> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|r| r.started_at_ms)
            .collect();
        assert_eq!(order, vec![300, 200, 100]);
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let raw = serde_json::json!({
            "schema_version": 999,
            "flow_id": "f",
            "name": "n",
            "strategy": "single",
            "started_at_ms": 1
        })
        .to_string();
        let err = deserialize_record(&raw).unwrap_err();
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }

    #[test]
    fn missing_schema_version_loads_as_v1() {
        let raw = serde_json::json!({
            "flow_id": "f",
            "name": "n",
            "strategy": "single",
            "started_at_ms": 1
        })
        .to_string();
        let record = deserialize_record(&raw).unwrap();
        assert_eq!(record.schema_version, SCHEMA_VERSION);
        assert!(!record.finished);
        assert!(record.outcome.is_none());
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let dir = tempdir().unwrap();
        let store = FlowStore::new(dir.path().to_path_buf());
        for bad in ["../escape", "a/b", "", "dots.here"] {
            assert!(
                store.write_def(bad, &single_def("x")).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let dir = tempdir().unwrap();
        let store = FlowStore::new(dir.path().join("flows"));
        store.write_def("job-1", &single_def("x")).unwrap();
        let flow_dir = dir.path().join("flows").join("job-1");
        let temps: Vec<_> = fs::read_dir(&flow_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(temps.is_empty(), "no temp file should remain after rename");
    }
}
