//! Daemon-adapter job lifecycle state.
//!
//! `nerve-runtime` owns the Rust schema for the protocol types and method contract. This module owns
//! only the runtime daemon's local lifecycle mechanics: in-memory retention,
//! thread spawning, event emission wiring, and cooperative cancellation tokens.
//!
//! The command executors are split by responsibility into child modules — [`agent`]
//! (`agent.run`), [`delegate`] (`delegate.*` + L0 run capture), [`substrate`] (the
//! L-series `run`/`replay`/`ledger`/`verify`/`policy`/`receipt`/`outcome` families),
//! [`flow`] (`flow.*`), and [`host`] (`host.*` / `workspace.*`). Each child file holds
//! `impl JobManager` method blocks; because privacy is visible-to-descendants, they
//! reach this module's private [`JobManager`] fields directly. [`run_job`](JobManager::run_job)
//! stays the single dispatcher, routing to the per-family `run_*_command` methods.

mod agent;
mod delegate;
mod flow;
mod host;
mod substrate;
mod wechat;

use crate::auth::AuthManager;
use crate::delegate_live::LiveSessions;
use crate::flow_job::LiveFlows;
use crate::policy::Policy;
use crate::sandbox::SandboxLauncher;
use crate::session::SessionStore;
use crate::session_manager::SessionManager;
use crate::{providers::ProviderRegistry, tools};
use nerve_core::CancelToken;
use nerve_runtime::{
    RuntimeCommand, RuntimeEvent, RuntimeJobError, RuntimeJobErrorExt, RuntimeJobGetRequest,
    RuntimeJobListRequest, RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_JOB_ID_LEN: usize = 128;
const TERMINAL_RETAINED: usize = 128;
const MAX_LIST_LIMIT: usize = 500;

pub(crate) type EventEmitter = dyn Fn(RuntimeEvent) + Send + Sync + 'static;

pub(crate) struct JobManager {
    runtime: Arc<tools::NerveRuntime>,
    registry: ProviderRegistry,
    /// Authorization policy for agent tool calls, resolved once at daemon
    /// startup. The daemon always pairs it with a deny-on-`Ask` approver.
    policy: Policy,
    /// Where `agent.run` transcripts are persisted (P5). `None` disables
    /// persistence (e.g. when no sessions dir could be resolved).
    session_store: Option<SessionStore>,
    /// Host executor for the protocol `session.*` command family.
    sessions: SessionManager,
    /// Host executor for the protocol `auth.*` command family.
    auth: AuthManager,
    /// Containment backend for `delegate.start` jobs, bound to the **trust
    /// context** like [`agent::AgentRunConfig::exec_launcher`]: a refusing
    /// launcher unless the daemon was started with `--allow-delegate`, so a
    /// served daemon refuses to spawn external agents by default.
    delegate_launcher: Arc<dyn SandboxLauncher>,
    /// Whether CLI workers (and `delegate_agent`) are lifted: the daemon's
    /// `--allow-delegate` flag. A `flow.start` whose nodes include CLI workers needs
    /// this lift (provider-worker flows do not), mirroring the delegate job path.
    allow_delegate: bool,
    /// Live, steerable delegated sessions (DA-5a), keyed by their `delegate.start`
    /// job id. A `claude` start job registers its session here and parks; later
    /// `delegate.steer` / `delegate.close` commands route through this registry.
    live_sessions: LiveSessions,
    /// Live + recently-finished flows (C2), keyed by their `flow.start` job id (the
    /// `flow_id`). `flow.get` / `flow.list` / `flow.close` route through here.
    flows: LiveFlows,
    /// Daemon-hosted WeChat bridge (`wechat.*` family): the logged-in iLink session
    /// plus the long-poll bridge thread that drives delegated turns in-process.
    wechat: Arc<crate::wechat::WechatHost>,
    jobs: Mutex<JobStore>,
    next_id: AtomicU64,
    emit: Arc<EventEmitter>,
}

#[derive(Default)]
struct JobStore {
    records: HashMap<String, JobRecord>,
    terminal_order: VecDeque<String>,
}

struct JobRecord {
    job_id: String,
    status: RuntimeJobStatus,
    command: String,
    tool_name: Option<String>,
    token: CancelToken,
    created_seq: u64,
    created_at_ms: u64,
    started_at_ms: Option<u64>,
    updated_at_ms: u64,
    finished_at_ms: Option<u64>,
    cancel_requested: bool,
    result: Option<Value>,
    error: Option<RuntimeJobError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobError {
    InvalidJobId(String),
    DuplicateJobId(String),
    UnknownJob(String),
}

impl JobError {
    #[must_use]
    pub(crate) fn code(&self) -> i64 {
        match self {
            Self::InvalidJobId(_) => -32602,
            Self::UnknownJob(_) => -32004,
            Self::DuplicateJobId(_) => -32009,
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJobId(job_id) => write!(f, "invalid job id: {job_id}"),
            Self::DuplicateJobId(job_id) => write!(f, "duplicate job id: {job_id}"),
            Self::UnknownJob(job_id) => write!(f, "unknown job: {job_id}"),
        }
    }
}

impl JobManager {
    /// Construct a [`JobManager`] that refuses delegation (the default daemon
    /// trust context). Test-only convenience over [`with_delegate_launcher`];
    /// production builds the router with an explicit launcher chosen by
    /// `--allow-delegate`.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new(
        runtime: Arc<tools::NerveRuntime>,
        registry: ProviderRegistry,
        policy: Policy,
        session_store: Option<SessionStore>,
        emit: impl Fn(RuntimeEvent) + Send + Sync + 'static,
    ) -> Self {
        Self::with_delegate_launcher(
            runtime,
            registry,
            policy,
            session_store,
            false,
            crate::sandbox::refuse_launcher(),
            emit,
        )
    }

    /// Construct a [`JobManager`] with an explicit delegate launcher. The daemon
    /// composition root injects a real (non-refusing) launcher here only when
    /// `--allow-delegate` is set; otherwise it passes a refusing launcher, so a
    /// served daemon refuses delegation. Scoping the lift to the delegate launcher
    /// keeps the agent-run / session exec posture (which refuses by trust context)
    /// untouched.
    #[must_use]
    pub(crate) fn with_delegate_launcher(
        runtime: Arc<tools::NerveRuntime>,
        registry: ProviderRegistry,
        policy: Policy,
        session_store: Option<SessionStore>,
        allow_delegate: bool,
        delegate_launcher: Arc<dyn SandboxLauncher>,
        emit: impl Fn(RuntimeEvent) + Send + Sync + 'static,
    ) -> Self {
        let emit: Arc<EventEmitter> = Arc::new(emit);
        // The session chat-tool path (DA-3) shares the daemon's `--allow-delegate`
        // lift and its delegate launcher with the `delegate.start` job path (DA-2),
        // so `nerve chat`'s spawned daemon enables `delegate_agent` in session turns.
        let sessions = SessionManager::new(
            Arc::clone(&runtime),
            registry.clone(),
            policy.clone(),
            session_store.clone(),
            Arc::clone(&emit),
            allow_delegate,
            Arc::clone(&delegate_launcher),
        );
        Self {
            runtime,
            registry,
            policy,
            session_store,
            sessions,
            auth: AuthManager::new(Arc::clone(&emit)),
            delegate_launcher,
            allow_delegate,
            live_sessions: LiveSessions::default(),
            flows: LiveFlows::default(),
            wechat: Arc::new(crate::wechat::WechatHost::new(Arc::clone(&emit))),
            jobs: Mutex::new(JobStore::default()),
            next_id: AtomicU64::new(1),
            emit,
        }
    }

    #[must_use]
    pub(crate) fn runtime(&self) -> &tools::NerveRuntime {
        &self.runtime
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        request: RuntimeJobStartRequest,
    ) -> Result<RuntimeJobSnapshot, JobError> {
        let command = request.command;
        let (job_id, created_seq) = self.resolve_job_id(request.job_id)?;
        let token = CancelToken::new();
        let record = JobRecord::new(job_id.clone(), created_seq, &command, token.clone());
        let snapshot = {
            let mut store = crate::sync::lock_recover(&self.jobs);
            if store.records.contains_key(&job_id) {
                return Err(JobError::DuplicateJobId(job_id));
            }
            let snapshot = record.snapshot(true);
            store.records.insert(job_id.clone(), record);
            snapshot
        };

        self.emit(RuntimeEvent::job_started(job_id.clone(), &command));
        let manager = Arc::clone(self);
        std::thread::spawn(move || manager.run_job(job_id, command, token));
        Ok(snapshot)
    }

    pub(crate) fn get(
        &self,
        request: RuntimeJobGetRequest,
    ) -> Result<RuntimeJobSnapshot, JobError> {
        let store = crate::sync::lock_recover(&self.jobs);
        let record = store
            .records
            .get(&request.job_id)
            .ok_or_else(|| JobError::UnknownJob(request.job_id.clone()))?;
        Ok(record.snapshot(request.include_result))
    }

    pub(crate) fn list(&self, request: RuntimeJobListRequest) -> Vec<RuntimeJobSnapshot> {
        let limit = request.limit.min(MAX_LIST_LIMIT);
        let store = crate::sync::lock_recover(&self.jobs);
        let mut records: Vec<_> = store
            .records
            .values()
            .filter(|record| request.include_terminal || !record.is_terminal())
            .collect();
        records.sort_by_key(|record| record.created_seq);
        records
            .into_iter()
            .take(limit)
            .map(|record| record.snapshot(request.include_results))
            .collect()
    }

    pub(crate) fn cancel(&self, job_id: &str) -> Result<(bool, RuntimeJobSnapshot), JobError> {
        let mut should_emit = false;
        let (requested, snapshot) = {
            let mut store = crate::sync::lock_recover(&self.jobs);
            let record = store
                .records
                .get_mut(job_id)
                .ok_or_else(|| JobError::UnknownJob(job_id.to_string()))?;
            if record.status == RuntimeJobStatus::Running {
                record.status = RuntimeJobStatus::Cancelling;
                record.cancel_requested = true;
                record.updated_at_ms = now_ms();
                record.token.cancel();
                should_emit = true;
                (true, record.snapshot(true))
            } else {
                (false, record.snapshot(true))
            }
        };
        if should_emit {
            // A cancelled `delegate.start` job may be parked on a live session
            // (DA-5a). The cancel token alone won't wake the parked thread (it
            // waits on the close condvar), so also request close: the parked
            // thread then force-kills the child and finishes as cancelled.
            let _ = self.live_sessions.close(job_id);
            self.emit(RuntimeEvent::job_cancel_requested(job_id.to_string()));
        }
        Ok((requested, snapshot))
    }

    fn resolve_job_id(&self, requested: Option<String>) -> Result<(String, u64), JobError> {
        if let Some(job_id) = requested {
            validate_job_id(&job_id)?;
            let created_seq = self.next_id.fetch_add(1, Ordering::Relaxed);
            return Ok((job_id, created_seq));
        }
        let created_seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        Ok((format!("job-{created_seq}"), created_seq))
    }

    fn run_job(self: Arc<Self>, job_id: String, command: RuntimeCommand, token: CancelToken) {
        self.emit(RuntimeEvent::job_progress(
            job_id.clone(),
            "executing",
            "executing runtime command",
            None,
            None,
        ));
        // Executor routing — every command is claimed by exactly one executor:
        // the agent.run job, the session manager, the auth manager, or the core
        // Runtime hub. `executor_for` is an *exhaustive* match on `RuntimeCommand`
        // (§10 hard gate): a new variant fails to COMPILE until it is mapped here,
        // so a command can never silently fall through to the hub. The
        // `command_executor_partition` test then asserts the mapping is total over
        // `RUNTIME_COMMAND_NAMES`.
        let outcome = match executor_for(&command) {
            Executor::AgentRun => self.run_agent_command(&job_id, command, &token),
            Executor::Delegate => self.run_delegate_command(&job_id, command, &token),
            Executor::Run => self.run_run_command(command),
            Executor::Replay => self.run_replay_command(&job_id, command, &token),
            Executor::Ledger => self.run_ledger_command(command),
            Executor::Verify => self.run_verify_command(command, &token),
            Executor::Policy => self.run_policy_command(command),
            Executor::Receipt => self.run_receipt_command(command),
            Executor::Outcome => self.run_outcome_command(command),
            Executor::Host => self.run_host_command(command, &token),
            Executor::Session => self.sessions.handle_command(command, &token),
            Executor::Auth => self.auth.handle_command(command, &token),
            Executor::Flow => self.run_flow_command(&job_id, command, &token),
            Executor::Wechat => self.run_wechat_command(command, &token),
            Executor::CoreHub => self.runtime.handle_command_cancellable(command, &token),
        };
        let event = {
            let mut store = crate::sync::lock_recover(&self.jobs);
            let Some(record) = store.records.get_mut(&job_id) else {
                return;
            };
            let event = record.finish(outcome);
            store.terminal_order.push_back(job_id.clone());
            prune_terminal_jobs(&mut store);
            event
        };
        self.emit(event);
    }

    fn emit(&self, event: RuntimeEvent) {
        (self.emit)(event);
    }
}

impl JobRecord {
    fn new(job_id: String, created_seq: u64, command: &RuntimeCommand, token: CancelToken) -> Self {
        let now = now_ms();
        Self {
            job_id,
            status: RuntimeJobStatus::Running,
            command: command.name().to_string(),
            tool_name: command.tool_name().map(str::to_string),
            token,
            created_seq,
            created_at_ms: now,
            started_at_ms: Some(now),
            updated_at_ms: now,
            finished_at_ms: None,
            cancel_requested: false,
            result: None,
            error: None,
        }
    }

    fn snapshot(&self, include_result: bool) -> RuntimeJobSnapshot {
        RuntimeJobSnapshot {
            job_id: self.job_id.clone(),
            status: self.status,
            command: self.command.clone(),
            tool_name: self.tool_name.clone(),
            created_at_ms: self.created_at_ms,
            started_at_ms: self.started_at_ms,
            updated_at_ms: self.updated_at_ms,
            finished_at_ms: self.finished_at_ms,
            cancel_requested: self.cancel_requested,
            result: include_result.then(|| self.result.clone()).flatten(),
            error: self.error.clone(),
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            RuntimeJobStatus::Completed | RuntimeJobStatus::Failed | RuntimeJobStatus::Cancelled
        )
    }

    fn finish(&mut self, outcome: Result<Value, nerve_runtime::RuntimeError>) -> RuntimeEvent {
        let now = now_ms();
        self.updated_at_ms = now;
        self.finished_at_ms = Some(now);
        match outcome {
            Ok(result) => {
                self.status = RuntimeJobStatus::Completed;
                self.result = Some(result);
                RuntimeEvent::job_completed(self.job_id.clone())
            }
            Err(error) if error.is_cancelled() => {
                self.status = RuntimeJobStatus::Cancelled;
                self.error = Some(RuntimeJobError::from_runtime_error(&error));
                RuntimeEvent::job_cancelled(self.job_id.clone())
            }
            Err(error) => {
                let job_error = RuntimeJobError::from_runtime_error(&error);
                self.status = RuntimeJobStatus::Failed;
                self.error = Some(job_error.clone());
                RuntimeEvent::job_failed(self.job_id.clone(), job_error)
            }
        }
    }
}

fn prune_terminal_jobs(store: &mut JobStore) {
    while store.terminal_order.len() > TERMINAL_RETAINED {
        let Some(job_id) = store.terminal_order.pop_front() else {
            return;
        };
        if store
            .records
            .get(&job_id)
            .is_some_and(JobRecord::is_terminal)
        {
            store.records.remove(&job_id);
        }
    }
}

fn validate_job_id(job_id: &str) -> Result<(), JobError> {
    if job_id.is_empty() || job_id.len() > MAX_JOB_ID_LEN {
        return Err(JobError::InvalidJobId(job_id.to_string()));
    }
    if job_id.bytes().all(is_valid_job_id_byte) {
        Ok(())
    } else {
        Err(JobError::InvalidJobId(job_id.to_string()))
    }
}

fn is_valid_job_id_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
}

/// The single executor that owns a [`RuntimeCommand`]. The `run_job` dispatch and
/// the §10 totality test both route through [`executor_for`], so this enum is the
/// one place the command→executor partition is defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Executor {
    /// The composition-root `agent.run` job (LLM orchestration).
    AgentRun,
    /// The host delegate runtime (`delegate.*` family): drives an external agent
    /// CLI subprocess. DA-1 ships a stub; DA-2 wires the real subprocess.
    Delegate,
    /// The host L0 run store (`run.*` family): enumerate/fetch captured Runs from
    /// the persisted [`RunStore`](crate::run_store) (read-only).
    Run,
    /// L0c deterministic replay (`replay.start`): re-drive a captured Run's tape and
    /// verify its content address against the recording.
    Replay,
    /// L1 evidence ledger (`ledger.query`): query the append-only cross-run log.
    Ledger,
    /// L2 execution-grounded verify (`verify.*`): re-run the org's checks in the
    /// pinned closure and seal/fetch the borrowed verdict.
    Verify,
    /// L3 policy plane (`policy.*`): serve the sealed policy doc + decision evidence.
    Policy,
    /// L4 receipt store (`receipt.get`): fetch a signed Verification Receipt.
    Receipt,
    /// L6 outcome corpus (`outcome.*`): append/get/query human/CI outcome labels.
    Outcome,
    /// The runtime host/native shell side-effects (`host.*` / `workspace.*`).
    Host,
    /// The host `SessionManager` (`session.*` family).
    Session,
    /// The host `AuthManager` (`auth.*` family).
    Auth,
    /// The host flow engine (`flow.*` family, C2): runs the deterministic C1
    /// orchestration engine as a job + the live-flow registry + approval routing.
    Flow,
    /// The daemon-hosted WeChat bridge (`wechat.*` family): QR login + the long-poll
    /// bridge that drives delegated turns in-process.
    Wechat,
    /// The core `Runtime` hub — nerve-core dispatch (`ping` / `tool.*`).
    CoreHub,
}

/// Map every protocol command to its single owning executor.
///
/// This is an **exhaustive** match on [`RuntimeCommand`] on purpose: it is the §10
/// hard gate. Adding a new variant breaks this match at COMPILE time, forcing an
/// explicit executor decision rather than letting the command fall through to the
/// core hub by default. Do not add a wildcard arm.
fn executor_for(command: &RuntimeCommand) -> Executor {
    match command {
        RuntimeCommand::AgentRun { .. } => Executor::AgentRun,
        RuntimeCommand::DelegateStart { .. }
        | RuntimeCommand::DelegateSteer { .. }
        | RuntimeCommand::DelegateClose { .. }
        | RuntimeCommand::DelegateGet { .. }
        | RuntimeCommand::DelegateList => Executor::Delegate,
        RuntimeCommand::RunList
        | RuntimeCommand::RunGet { .. }
        | RuntimeCommand::OtelIngest { .. } => Executor::Run,
        RuntimeCommand::ReplayStart { .. } => Executor::Replay,
        RuntimeCommand::LedgerQuery { .. } => Executor::Ledger,
        RuntimeCommand::VerifyStart { .. }
        | RuntimeCommand::VerifyGet { .. }
        | RuntimeCommand::VerifyList { .. } => Executor::Verify,
        RuntimeCommand::PolicyGet | RuntimeCommand::PolicyDecisions { .. } => Executor::Policy,
        RuntimeCommand::ReceiptGet { .. } => Executor::Receipt,
        RuntimeCommand::OutcomeLabel { .. }
        | RuntimeCommand::OutcomeGet { .. }
        | RuntimeCommand::OutcomeQuery { .. } => Executor::Outcome,
        RuntimeCommand::HostCapabilities
        | RuntimeCommand::HostClipboardWriteText { .. }
        | RuntimeCommand::HostNotificationShow { .. }
        | RuntimeCommand::HostFolderPick { .. }
        | RuntimeCommand::HostFileSaveText { .. }
        | RuntimeCommand::HostUrlOpen { .. }
        | RuntimeCommand::WorkspaceReveal { .. } => Executor::Host,
        RuntimeCommand::SessionStart { .. }
        | RuntimeCommand::SessionMessage { .. }
        | RuntimeCommand::SessionInterrupt { .. }
        | RuntimeCommand::SessionRespond { .. }
        | RuntimeCommand::SessionGet { .. }
        | RuntimeCommand::SessionList
        | RuntimeCommand::SessionClose { .. }
        | RuntimeCommand::SessionSetModel { .. }
        | RuntimeCommand::SessionSetMode { .. } => Executor::Session,
        RuntimeCommand::AuthStart { .. }
        | RuntimeCommand::AuthComplete { .. }
        | RuntimeCommand::AuthStatus { .. }
        | RuntimeCommand::AuthLease { .. }
        | RuntimeCommand::AuthLogout { .. } => Executor::Auth,
        RuntimeCommand::FlowStart { .. }
        | RuntimeCommand::FlowSteer { .. }
        | RuntimeCommand::FlowReplay { .. }
        | RuntimeCommand::FlowGet { .. }
        | RuntimeCommand::FlowList
        | RuntimeCommand::FlowClose { .. }
        | RuntimeCommand::FlowRespond { .. } => Executor::Flow,
        RuntimeCommand::WechatLogin { .. }
        | RuntimeCommand::WechatStart { .. }
        | RuntimeCommand::WechatStop
        | RuntimeCommand::WechatStatus => Executor::Wechat,
        RuntimeCommand::Ping | RuntimeCommand::ToolList | RuntimeCommand::ToolCall { .. } => {
            Executor::CoreHub
        }
    }
}

fn now_ms() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod command_executor_partition {
    //! Governance test (architecture north star §10): the command-executor
    //! *totality* property, now backed by a **compile-time** hard gate.
    //! [`executor_for`] is an exhaustive match on [`RuntimeCommand`], so a new
    //! variant cannot compile until it is mapped to one [`Executor`] — there is no
    //! `else`/wildcard for it to fall through. These tests close the loop at
    //! run time: every name in the authoritative `RUNTIME_COMMAND_NAMES` maps to
    //! exactly one executor, so the wire vocabulary and the dispatch stay aligned
    //! (e.g. a `kind` rename that drifts from the constant is caught).
    use super::*;
    use serde_json::json;
    use std::collections::BTreeSet;

    /// Build a minimal representative value for a protocol command name by the
    /// real `kind`-tagged deserialization path (so a `kind` rename that drifts
    /// from `RUNTIME_COMMAND_NAMES` is caught too). Panics on an unknown name, so
    /// adding a name without teaching this test fails loudly — which then forces
    /// an executor decision in `every_runtime_command_has_exactly_one_executor`.
    fn representative(name: &str) -> RuntimeCommand {
        let fields: Value = match name {
            "ping" | "tool.list" | "session.list" | "flow.list" | "delegate.list" | "run.list"
            | "host.capabilities" | "workspace.reveal" => json!({}),
            "run.get" => json!({ "run_id": "r" }),
            "host.clipboard.write_text" => json!({ "text": "copy me" }),
            "host.notification.show" => json!({ "title": "Nerve", "body": "Done" }),
            "host.folder.pick" => json!({ "title": "Choose project folder" }),
            "host.file.save_text" => json!({
                "title": "Save packet",
                "default_name": "packet.md",
                "text": "# Packet"
            }),
            "host.url.open" => json!({ "url": "https://example.com/auth" }),
            "tool.call" => json!({ "name": "file_search" }),
            "agent.run" => json!({ "provider": "p", "model": "m", "task": "t" }),
            "session.start" => json!({ "provider": "p", "model": "m" }),
            "session.message" => json!({ "session_id": "s", "text": "t" }),
            "session.interrupt" | "session.get" | "session.close" => json!({ "session_id": "s" }),
            "session.respond" => {
                json!({ "session_id": "s", "request_id": "r", "decision": "allow" })
            }
            "session.set_model" => json!({ "session_id": "s", "model": "m" }),
            "session.set_mode" => json!({ "session_id": "s", "mode": "yolo" }),
            "auth.start" => json!({ "provider": "p", "flow": "browser" }),
            "auth.status" | "auth.logout" => json!({ "provider": "p" }),
            "auth.lease" => {
                json!({ "provider": "p", "force_refresh": false, "include_token": false })
            }
            "auth.complete" => json!({ "login_id": "l" }),
            "delegate.start" => json!({ "agent": "codex", "task": "t" }),
            "delegate.steer" => json!({ "session_id": "s", "message": "m" }),
            "delegate.close" | "delegate.get" => json!({ "session_id": "s" }),
            "flow.start" => json!({
                "workflow": {
                    "schema_version": 1,
                    "name": "n",
                    "strategy": {
                        "type": "single",
                        "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "t" }
                    }
                }
            }),
            "flow.steer" => json!({ "flow_id": "f", "message": "m" }),
            "flow.replay" => json!({ "flow_id": "f" }),
            "flow.get" | "flow.close" => json!({ "flow_id": "f" }),
            "flow.respond" => json!({ "flow_id": "f", "request_id": "r", "decision": "allow" }),
            "wechat.login" | "wechat.start" | "wechat.stop" | "wechat.status" => json!({}),
            "replay.start" => json!({ "run_id": "r" }),
            "ledger.query" | "policy.get" | "policy.decisions" | "verify.list" => json!({}),
            "verify.start" => json!({ "run_id": "r" }),
            "verify.get" => json!({ "verdict_id": "v" }),
            "receipt.get" => json!({ "receipt_id": "r" }),
            "otel.ingest" => json!({ "trace": {} }),
            "outcome.label" => json!({
                "run_id": "r",
                "outcome": { "outcome": "merged" },
                "source": { "source": "human" }
            }),
            "outcome.get" => json!({ "run_id": "r" }),
            "outcome.query" => json!({}),
            other => panic!(
                "RUNTIME_COMMAND_NAMES gained `{other}` with no representative here; add one and \
                 wire the variant to exactly one executor in `run_job`"
            ),
        };
        let mut object = fields.as_object().cloned().unwrap_or_default();
        object.insert("kind".to_string(), Value::String(name.to_string()));
        serde_json::from_value(Value::Object(object))
            .unwrap_or_else(|err| panic!("representative `{name}` failed to deserialize: {err}"))
    }

    #[test]
    fn every_runtime_command_maps_to_one_executor() {
        // `executor_for` is exhaustive (total) by construction — it would not
        // compile otherwise. This asserts the *name* table agrees: each protocol
        // name builds a command whose kind round-trips and resolves to exactly one
        // executor. A name added without a representative panics in `representative`
        // (and a kind drift is caught by the `name()` equality below).
        let mut seen_per_executor: HashMap<Executor, Vec<&str>> = HashMap::new();
        for &name in nerve_runtime::RUNTIME_COMMAND_NAMES {
            let command = representative(name);
            assert_eq!(
                command.name(),
                name,
                "representative for `{name}` built the wrong command kind"
            );
            // The match is exhaustive, so `executor_for` always returns exactly one
            // executor — no command can be unclaimed or double-claimed.
            let executor = executor_for(&command);
            seen_per_executor.entry(executor).or_default().push(name);
        }
        // Every executor must own at least one command (none is dead), and the
        // union of owned names must cover the whole vocabulary exactly once.
        let total: usize = seen_per_executor.values().map(Vec::len).sum();
        assert_eq!(
            total,
            nerve_runtime::RUNTIME_COMMAND_NAMES.len(),
            "executor map did not cover every command exactly once: {seen_per_executor:?}"
        );
        for executor in [
            Executor::AgentRun,
            Executor::Delegate,
            Executor::Run,
            Executor::Replay,
            Executor::Ledger,
            Executor::Verify,
            Executor::Policy,
            Executor::Receipt,
            Executor::Outcome,
            Executor::Host,
            Executor::Session,
            Executor::Auth,
            Executor::Flow,
            Executor::Wechat,
            Executor::CoreHub,
        ] {
            assert!(
                seen_per_executor.contains_key(&executor),
                "executor {executor:?} owns no command — dead executor arm in `run_job`"
            );
        }
    }

    #[test]
    fn executor_for_routes_each_family_to_its_owner() {
        // Spot-check the routing is by *family*, not incidental, so a misfiled
        // variant (e.g. an auth command routed to the session manager) is caught.
        assert_eq!(executor_for(&representative("ping")), Executor::CoreHub);
        assert_eq!(
            executor_for(&representative("tool.call")),
            Executor::CoreHub
        );
        assert_eq!(
            executor_for(&representative("agent.run")),
            Executor::AgentRun
        );
        assert_eq!(
            executor_for(&representative("session.start")),
            Executor::Session
        );
        assert_eq!(executor_for(&representative("auth.start")), Executor::Auth);
        for name in [
            "host.capabilities",
            "host.clipboard.write_text",
            "host.notification.show",
            "host.folder.pick",
            "host.file.save_text",
            "host.url.open",
            "workspace.reveal",
        ] {
            assert_eq!(
                executor_for(&representative(name)),
                Executor::Host,
                "`{name}` must route to the host executor"
            );
        }
        for name in [
            "flow.start",
            "flow.steer",
            "flow.replay",
            "flow.get",
            "flow.list",
            "flow.close",
            "flow.respond",
        ] {
            assert_eq!(
                executor_for(&representative(name)),
                Executor::Flow,
                "`{name}` must route to the flow executor"
            );
        }
    }

    #[test]
    fn runtime_command_names_have_no_duplicates() {
        let unique: BTreeSet<_> = nerve_runtime::RUNTIME_COMMAND_NAMES.iter().collect();
        assert_eq!(
            unique.len(),
            nerve_runtime::RUNTIME_COMMAND_NAMES.len(),
            "RUNTIME_COMMAND_NAMES contains duplicate entries"
        );
    }
}
