//! P5 — durable session/transcript persistence (architecture north star §7.6
//! "persistence schema", roadmap §8 P5). Each `nerve agent` run is recorded, at
//! the composition root, as a versioned JSON transcript so runs survive process
//! exit — the daemon's in-memory jobs (`jobs.rs`) are pruned and vanish when the
//! daemon stops.
//!
//! Seam discipline: this lives entirely in the binary. `nerve-agent` and its
//! `Orchestrator` are untouched — `run_agent` wraps the event sink so it both
//! streams to the caller and records here. The persisted shape ([`SessionRecord`])
//! is owned by this module rather than re-using agent/runtime domain types, so
//! the on-disk schema can evolve independently, gated by `schema_version` and a
//! tolerant [`load`](SessionStore::load) path.
//!
//! Out of scope (deliberately): **resume** — seeding a fresh run from a past
//! transcript needs the orchestrator to accept prior history, which is the future
//! Session layer (roadmap P0). Here we only persist and browse.

use anyhow::{Context, Result, anyhow};
use nerve_agent::{AgentEvent, Message, RunOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Current on-disk transcript schema. Bump when [`SessionRecord`] changes shape,
/// and add a migration arm in [`migrate_to_current`] for the previous version.
const SCHEMA_VERSION: u32 = 1;
#[allow(dead_code)]
const CHECKPOINT_STALENESS_MARKER: &str =
    "[restored from a prior session — update or clear if the task changed]";

/// A persisted agent run: metadata, the streamed events, and the final outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionRecord {
    /// On-disk schema version, for migration. See [`SCHEMA_VERSION`].
    pub(crate) schema_version: u32,
    /// Stable, timestamp-derived id; also the file stem (`<id>.json`).
    pub(crate) id: String,
    /// Unix-epoch milliseconds when the run started.
    pub(crate) started_at_ms: u64,
    /// Unix-epoch milliseconds when the run finished (None if never finalized).
    #[serde(default)]
    pub(crate) finished_at_ms: Option<u64>,
    /// Provider name (built-in alias or config entry) the run used.
    pub(crate) provider: String,
    /// Model id the run used.
    pub(crate) model: String,
    /// The task prompt the agent was given.
    pub(crate) task: String,
    /// Provider-neutral conversation history. Added for the interactive
    /// Session layer; older one-shot transcripts omit it and are reconstructed
    /// best-effort from `task` + `outcome` when resumed.
    #[serde(default)]
    pub(crate) history: Vec<Message>,
    /// Working-memory checkpoint restored into resumed sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkpoint: Option<String>,
    /// The streamed transcript, in order.
    #[serde(default)]
    pub(crate) events: Vec<SessionEvent>,
    /// The terminal outcome, present once the run finished successfully.
    #[serde(default)]
    pub(crate) outcome: Option<SessionOutcome>,
}

/// Serializable mirror of [`nerve_agent::AgentEvent`] (which is not itself
/// `Serialize`). Tagged so the transcript is self-describing and diff-friendly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum SessionEvent {
    /// A new turn started (1-based index).
    TurnStarted { turn: u32 },
    /// Assistant output text (consecutive chunks are coalesced).
    AssistantText { text: String },
    /// Reasoning/thinking text (consecutive chunks are coalesced).
    Reasoning { text: String },
    /// A tool invocation began.
    ToolStarted { name: String, arguments: Value },
    /// A tool invocation finished.
    ToolFinished {
        name: String,
        ok: bool,
        output: String,
    },
    /// The run was interrupted (cancellation or guardrail).
    Interrupted { reason: String },
    /// The run reached a terminal state.
    Done { reason: String },
}

/// Serializable mirror of [`nerve_agent::RunOutcome`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionOutcome {
    /// Terminal reason (e.g. "stop", "max_turns", "cancelled").
    pub(crate) reason: String,
    /// Number of turns executed.
    pub(crate) turns: u32,
    /// Final assistant text.
    pub(crate) final_text: String,
    /// Aggregate token usage across the run.
    pub(crate) usage: SessionUsage,
}

/// Token accounting, mirrored so the on-disk schema is self-owned.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct SessionUsage {
    pub(crate) input_tokens: u32,
    pub(crate) output_tokens: u32,
}

impl SessionRecord {
    /// Begin a fresh record, stamped with a new id and the current time.
    pub(crate) fn begin(provider: &str, model: &str, task: &str) -> Self {
        let started_at_ms = now_ms();
        Self {
            schema_version: SCHEMA_VERSION,
            id: new_session_id(started_at_ms),
            started_at_ms,
            finished_at_ms: None,
            provider: provider.to_string(),
            model: model.to_string(),
            task: task.to_string(),
            history: Vec::new(),
            checkpoint: None,
            events: Vec::new(),
            outcome: None,
        }
    }

    /// Replace the stored provider-neutral conversation history.
    pub(crate) fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// Replace the stored working-memory checkpoint.
    #[allow(dead_code)]
    pub(crate) fn set_checkpoint(&mut self, checkpoint: Option<String>) {
        self.checkpoint = checkpoint;
    }

    /// Checkpoint for resume, marked stale so the agent refreshes or clears it.
    #[allow(dead_code)]
    pub(crate) fn restore_with_staleness(&self) -> Option<String> {
        self.checkpoint
            .as_ref()
            .map(|checkpoint| format!("{checkpoint}\n{CHECKPOINT_STALENESS_MARKER}"))
    }

    /// Conversation history for resume. New interactive-session records return
    /// the exact provider-neutral messages; older one-shot records get a
    /// best-effort reconstruction from the original task and final assistant text.
    pub(crate) fn reconstructed_history(&self) -> Vec<Message> {
        if !self.history.is_empty() {
            return self.history.clone();
        }
        let mut history = vec![Message::user(self.task.clone())];
        if let Some(outcome) = &self.outcome
            && !outcome.final_text.is_empty()
        {
            history.push(Message::assistant(outcome.final_text.clone()));
        }
        history
    }

    /// Append one streamed event, coalescing consecutive assistant-text and
    /// reasoning chunks so the transcript stays compact and readable.
    pub(crate) fn push_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AssistantText(text) => {
                if let Some(SessionEvent::AssistantText { text: prev }) = self.events.last_mut() {
                    prev.push_str(text);
                } else {
                    self.events
                        .push(SessionEvent::AssistantText { text: text.clone() });
                }
            }
            AgentEvent::Reasoning(text) => {
                if let Some(SessionEvent::Reasoning { text: prev }) = self.events.last_mut() {
                    prev.push_str(text);
                } else {
                    self.events
                        .push(SessionEvent::Reasoning { text: text.clone() });
                }
            }
            AgentEvent::TurnStarted(turn) => {
                self.events.push(SessionEvent::TurnStarted { turn: *turn });
            }
            AgentEvent::ToolStarted { name, args } => self.events.push(SessionEvent::ToolStarted {
                name: name.clone(),
                arguments: args.clone(),
            }),
            AgentEvent::ToolFinished { name, ok, output } => {
                self.events.push(SessionEvent::ToolFinished {
                    name: name.clone(),
                    ok: *ok,
                    output: output.clone(),
                });
            }
            AgentEvent::Interrupted(reason) => self.events.push(SessionEvent::Interrupted {
                reason: reason.clone(),
            }),
            // Per-turn token usage is surfaced live via the protocol; the
            // transcript keeps only the final total in the outcome.
            AgentEvent::Usage { .. } => {}
            AgentEvent::Done { reason } => self.events.push(SessionEvent::Done {
                reason: reason.clone(),
            }),
        }
    }

    /// Finalize: stamp the finish time and capture the outcome when the run
    /// produced one. Interrupted/failed runs persist their partial transcript
    /// with `outcome: None`.
    pub(crate) fn finish(&mut self, outcome: Option<&RunOutcome>) {
        self.finished_at_ms = Some(now_ms());
        self.outcome = outcome.map(|outcome| SessionOutcome {
            reason: outcome.reason.clone(),
            turns: outcome.turns,
            final_text: outcome.final_text.clone(),
            usage: SessionUsage {
                input_tokens: outcome.usage.input_tokens,
                output_tokens: outcome.usage.output_tokens,
            },
        });
    }

    /// One-line summary for `nerve agent sessions list`.
    pub(crate) fn summary_line(&self) -> String {
        let outcome = self
            .outcome
            .as_ref()
            .map_or("unfinished", |outcome| outcome.reason.as_str());
        format!(
            "{}  {}  {}/{}  [{}]  {}",
            self.id,
            format_human_utc(self.started_at_ms),
            self.provider,
            self.model,
            outcome,
            truncate(&one_line(&self.task), 64),
        )
    }

    /// Full, human-readable transcript for `nerve agent sessions show <id>`.
    pub(crate) fn render_transcript(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("session  {}\n", self.id));
        out.push_str(&format!(
            "started  {}\n",
            format_human_utc(self.started_at_ms)
        ));
        if let Some(finished) = self.finished_at_ms {
            out.push_str(&format!("finished {}\n", format_human_utc(finished)));
        }
        out.push_str(&format!("provider {}\n", self.provider));
        out.push_str(&format!("model    {}\n", self.model));
        out.push_str(&format!("task     {}\n\n--- transcript ---\n", self.task));
        for event in &self.events {
            out.push_str(&render_event(event));
        }
        if let Some(outcome) = &self.outcome {
            out.push_str(&format!(
                "\n--- outcome ---\nreason {}  turns {}  tokens {} in / {} out\n",
                outcome.reason,
                outcome.turns,
                outcome.usage.input_tokens,
                outcome.usage.output_tokens,
            ));
            if !outcome.final_text.is_empty() {
                out.push_str(&format!("\n{}\n", outcome.final_text));
            }
        }
        out
    }
}

/// Render a single transcript event as a line (or block) of text.
fn render_event(event: &SessionEvent) -> String {
    match event {
        SessionEvent::TurnStarted { turn } => format!("\n[turn {turn}]\n"),
        SessionEvent::AssistantText { text } => format!("{text}\n"),
        SessionEvent::Reasoning { text } => format!("(reasoning) {}\n", one_line(text)),
        SessionEvent::ToolStarted { name, arguments } => {
            format!("  -> {name} {}\n", truncate(&arguments.to_string(), 160))
        }
        SessionEvent::ToolFinished { name, ok, output } => {
            let mark = if *ok { "ok" } else { "ERR" };
            format!(
                "  <- {name} [{mark}] {}\n",
                truncate(&one_line(output), 160)
            )
        }
        SessionEvent::Interrupted { reason } => format!("\n[interrupted: {reason}]\n"),
        SessionEvent::Done { reason } => format!("\n[done: {reason}]\n"),
    }
}

/// A directory of session transcripts (`<dir>/<id>.json`).
#[derive(Clone)]
pub(crate) struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Wrap an explicit directory.
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Resolve the sessions directory for a run/browse scope: the project
    /// `<root>/.nerve/sessions` when a root is known, else the global
    /// `config_home()/sessions`.
    pub(crate) fn for_scope(root: Option<&Path>) -> Result<Self> {
        Ok(Self::new(resolve_sessions_dir(root)?))
    }

    /// The backing directory.
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Write `record` to `<dir>/<id>.json`, creating the directory as needed.
    pub(crate) fn write(&self, record: &SessionRecord) -> Result<PathBuf> {
        fs::create_dir_all(&self.dir)
            .with_context(|| format!("failed to create sessions dir {}", self.dir.display()))?;
        let path = self.dir.join(format!("{}.json", record.id));
        let json = serde_json::to_string_pretty(record).context("serialize session record")?;
        fs::write(&path, json).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    /// Raw stored JSON for `id` (for `sessions show --json`).
    pub(crate) fn read_raw(&self, id: &str) -> Result<String> {
        let path = self.path_for(id)?;
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))
    }

    /// Load and migrate the record for `id`.
    pub(crate) fn load(&self, id: &str) -> Result<SessionRecord> {
        let raw = self.read_raw(id)?;
        deserialize_record(&raw).with_context(|| format!("failed to parse session {id}"))
    }

    /// All stored records, most recent first.
    pub(crate) fn list(&self) -> Result<Vec<SessionRecord>> {
        let mut records = Vec::new();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(records),
            Err(err) => return Err(anyhow!("failed to read {}: {err}", self.dir.display())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            if let Ok(record) = deserialize_record(&raw) {
                records.push(record);
            }
        }
        records.sort_by(|a, b| {
            b.started_at_ms
                .cmp(&a.started_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(records)
    }

    fn path_for(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.dir.join(format!("{id}.json")))
    }
}

/// Parse raw transcript JSON, applying any schema migrations first. Tolerant of
/// an older/missing `schema_version` (treated as v1, with field defaults filling
/// gaps); rejects a version newer than this binary understands.
fn deserialize_record(raw: &str) -> Result<SessionRecord> {
    let mut value: Value = serde_json::from_str(raw).context("invalid session JSON")?;
    let version = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    migrate_to_current(&mut value, version)?;
    serde_json::from_value(value).context("session record shape mismatch")
}

/// Upgrade a transcript `value` from `version` to [`SCHEMA_VERSION`] in place.
/// Only one version exists today, so this is the newer-than-known guard plus a
/// version re-stamp — but the stepwise seam is here: add an arm per future bump
/// (e.g. `if version < 2 { /* transform v1 -> v2 */ }`), oldest-first.
fn migrate_to_current(value: &mut Value, version: u64) -> Result<()> {
    if version > u64::from(SCHEMA_VERSION) {
        return Err(anyhow!(
            "session schema_version {version} is newer than supported {SCHEMA_VERSION}; upgrade nerve"
        ));
    }
    // Future migrations land here, oldest-first.
    if let Some(object) = value.as_object_mut() {
        object.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
    }
    Ok(())
}

/// `<root>/.nerve/sessions` for a project root, else the global
/// `config_home()/sessions` when no root is known.
fn resolve_sessions_dir(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root.join(".nerve").join("sessions")),
        None => {
            let home = nerve_agent::auth::config_home().map_err(|err| anyhow!("{err}"))?;
            Ok(home.join("sessions"))
        }
    }
}

/// Reject ids that could escape the sessions directory. Ids are simple tokens —
/// ASCII alphanumerics plus `-`/`_` — so `<id>.json` always stays in-dir.
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
            "invalid session id '{id}': use only letters, digits, '-' and '_'"
        ))
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

/// Process-local disambiguator so two runs starting in the same second (daemon
/// concurrency) still get distinct ids.
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Timestamp-derived id `YYYYMMDDThhmmssZ-NNN` (UTC second resolution plus a
/// per-process counter). Lexically sortable and dependency-free — no uuid/rand.
fn new_session_id(now_ms: u64) -> String {
    let seq = SESSION_SEQ.fetch_add(1, Ordering::Relaxed) % 1000;
    format!("{}-{seq:03}", format_basic_utc(now_ms))
}

/// Civil `(year, month, day)` from a Unix day number — Howard Hinnant's
/// `civil_from_days`, pure integer math, no dependencies.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Split epoch-millis into `(year, month, day, hour, minute, second)` UTC.
fn ymd_hms(now_ms: u64) -> (i64, u32, u32, u32, u32, u32) {
    let secs = (now_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    (
        year,
        month,
        day,
        (tod / 3600) as u32,
        ((tod % 3600) / 60) as u32,
        (tod % 60) as u32,
    )
}

/// Compact basic-ISO stamp for ids, e.g. `20260618T120000Z`.
fn format_basic_utc(now_ms: u64) -> String {
    let (y, mo, d, h, mi, s) = ymd_hms(now_ms);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// Readable stamp for listings, e.g. `2026-06-18 12:00:00Z`.
fn format_human_utc(now_ms: u64) -> String {
    let (y, mo, d, h, mi, s) = ymd_hms(now_ms);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

/// Collapse newlines so a value renders on one line.
fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

/// Truncate to `max` characters, appending an ellipsis when shortened.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn sample(id: &str, started_at_ms: u64) -> SessionRecord {
        SessionRecord {
            schema_version: SCHEMA_VERSION,
            id: id.to_string(),
            started_at_ms,
            finished_at_ms: Some(started_at_ms + 5),
            provider: "claude".into(),
            model: "m-1".into(),
            task: "do the thing".into(),
            history: Vec::new(),
            checkpoint: None,
            events: vec![
                SessionEvent::TurnStarted { turn: 1 },
                SessionEvent::AssistantText {
                    text: "hello".into(),
                },
                SessionEvent::ToolFinished {
                    name: "read_file".into(),
                    ok: true,
                    output: "ok".into(),
                },
            ],
            outcome: Some(SessionOutcome {
                reason: "stop".into(),
                turns: 1,
                final_text: "done".into(),
                usage: SessionUsage {
                    input_tokens: 10,
                    output_tokens: 7,
                },
            }),
        }
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        let record = sample("20260618T120000Z-000", 1_000);
        let path = store.write(&record).unwrap();
        assert!(path.exists());

        let loaded = store.load(&record.id).unwrap();
        assert_eq!(loaded.schema_version, SCHEMA_VERSION);
        assert_eq!(loaded.id, record.id);
        assert_eq!(loaded.provider, "claude");
        assert_eq!(loaded.model, "m-1");
        assert_eq!(loaded.task, record.task);
        assert!(loaded.history.is_empty());
        assert_eq!(loaded.events.len(), 3);
        assert_eq!(loaded.outcome.unwrap().usage.output_tokens, 7);
    }

    #[test]
    fn for_scope_uses_project_nerve_sessions() {
        let store = SessionStore::for_scope(Some(Path::new("/tmp/proj"))).unwrap();
        assert_eq!(store.dir(), Path::new("/tmp/proj/.nerve/sessions"));
    }

    #[test]
    fn missing_schema_version_loads_as_v1() {
        // An older transcript without an explicit schema_version is tolerated,
        // and absent newer fields fall back to their defaults.
        let raw = json!({
            "id": "20260618T120000Z-000",
            "started_at_ms": 1,
            "provider": "claude",
            "model": "m",
            "task": "t"
        })
        .to_string();
        let record = deserialize_record(&raw).unwrap();
        assert_eq!(record.schema_version, SCHEMA_VERSION);
        assert!(record.events.is_empty());
        assert!(record.history.is_empty());
        assert!(record.outcome.is_none());
        assert!(record.finished_at_ms.is_none());
        assert!(record.checkpoint.is_none());
    }

    #[test]
    fn checkpoint_is_skipped_when_absent() {
        let record = sample("20260618T120000Z-000", 100);
        let value = serde_json::to_value(&record).unwrap();
        assert!(value.get("checkpoint").is_none());

        let round_trip: SessionRecord = serde_json::from_value(value).unwrap();
        assert!(round_trip.checkpoint.is_none());
    }

    #[test]
    fn checkpoint_round_trips_when_present() {
        let mut record = sample("20260618T120000Z-000", 100);
        record.set_checkpoint(Some("next: inspect session manager".into()));
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(
            value.get("checkpoint").and_then(Value::as_str),
            Some("next: inspect session manager")
        );

        let round_trip: SessionRecord = serde_json::from_value(value).unwrap();
        assert_eq!(
            round_trip.checkpoint.as_deref(),
            Some("next: inspect session manager")
        );
    }

    #[test]
    fn restore_with_staleness_appends_marker() {
        let mut record = sample("20260618T120000Z-000", 100);
        record.set_checkpoint(Some("remember this".into()));
        assert_eq!(
            record.restore_with_staleness().as_deref(),
            Some(
                "remember this\n[restored from a prior session — update or clear if the task changed]"
            )
        );
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let raw = json!({
            "schema_version": 999,
            "id": "x-0",
            "started_at_ms": 1,
            "provider": "p",
            "model": "m",
            "task": "t"
        })
        .to_string();
        let err = deserialize_record(&raw).unwrap_err();
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }

    #[test]
    fn history_round_trips_for_resume() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        let mut record = sample("20260618T120000Z-000", 100);
        record.set_history(vec![Message::user("hello"), Message::assistant("hi")]);
        store.write(&record).unwrap();

        let loaded = store.load(&record.id).unwrap();
        let history = loaded.reconstructed_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "hello");
        assert_eq!(history[1].content, "hi");
    }

    #[test]
    fn older_transcript_reconstructs_minimal_history() {
        let record = sample("20260618T120000Z-000", 100);
        let history = record.reconstructed_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "do the thing");
        assert_eq!(history[1].content, "done");
    }

    #[test]
    fn list_orders_most_recent_first() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        store.write(&sample("20260618T120000Z-000", 100)).unwrap();
        store.write(&sample("20260618T120100Z-001", 300)).unwrap();
        store.write(&sample("20260618T120030Z-002", 200)).unwrap();
        let order: Vec<u64> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|record| record.started_at_ms)
            .collect();
        assert_eq!(order, vec![300, 200, 100]);
    }

    #[test]
    fn list_tolerates_missing_dir() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("does-not-exist"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn push_event_coalesces_consecutive_text() {
        let mut record = SessionRecord::begin("p", "m", "t");
        record.push_event(&AgentEvent::AssistantText("foo ".into()));
        record.push_event(&AgentEvent::AssistantText("bar".into()));
        record.push_event(&AgentEvent::TurnStarted(2));
        record.push_event(&AgentEvent::AssistantText("baz".into()));
        assert_eq!(record.events.len(), 3);
        match &record.events[0] {
            SessionEvent::AssistantText { text } => assert_eq!(text, "foo bar"),
            other => panic!("unexpected first event: {other:?}"),
        }
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().to_path_buf());
        for bad in ["../escape", "a/b", "", "dots.here", "back\\slash"] {
            assert!(store.load(bad).is_err(), "expected '{bad}' to be rejected");
        }
    }

    #[test]
    fn summary_and_transcript_render() {
        let record = sample("20260618T120000Z-000", 1_000);
        let line = record.summary_line();
        assert!(line.contains("20260618T120000Z-000"));
        assert!(line.contains("claude/m-1"));
        assert!(line.contains("[stop]"));

        let text = record.render_transcript();
        assert!(text.contains("session  20260618T120000Z-000"));
        assert!(text.contains("read_file"));
        assert!(text.contains("reason stop"));
    }

    #[test]
    fn basic_utc_stamp_is_well_formed() {
        // 2026-06-18T12:00:00Z == 1_781_784_000 s since the epoch.
        assert_eq!(format_basic_utc(1_781_784_000_000), "20260618T120000Z");
        assert_eq!(format_human_utc(1_781_784_000_000), "2026-06-18 12:00:00Z");
    }
}
