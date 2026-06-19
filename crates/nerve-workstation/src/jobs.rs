//! Daemon-adapter job lifecycle state.
//!
//! `nerve-runtime` owns the Rust schema for the protocol types and method contract. This module owns
//! only the runtime daemon's local lifecycle mechanics: in-memory retention,
//! thread spawning, event emission wiring, and cooperative cancellation tokens.

use crate::auth::AuthManager;
use crate::policy::{Policy, ToolGate};
use crate::session::SessionStore;
use crate::session_manager::SessionManager;
use crate::{agent, providers::ProviderRegistry, tools};
use nerve_agent::AgentEvent;
use nerve_core::CancelToken;
use nerve_runtime::{
    RuntimeCommand, RuntimeEvent, RuntimeJobError, RuntimeJobGetRequest, RuntimeJobListRequest,
    RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_JOB_ID_LEN: usize = 128;
const TERMINAL_RETAINED: usize = 128;
const MAX_LIST_LIMIT: usize = 500;

type EventEmitter = dyn Fn(RuntimeEvent) + Send + Sync + 'static;

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
    #[must_use]
    pub(crate) fn new(
        runtime: Arc<tools::NerveRuntime>,
        registry: ProviderRegistry,
        policy: Policy,
        session_store: Option<SessionStore>,
        emit: impl Fn(RuntimeEvent) + Send + Sync + 'static,
    ) -> Self {
        let emit: Arc<EventEmitter> = Arc::new(emit);
        let sessions = SessionManager::new(
            Arc::clone(&runtime),
            registry.clone(),
            policy.clone(),
            session_store.clone(),
            Arc::clone(&emit),
        );
        Self {
            runtime,
            registry,
            policy,
            session_store,
            sessions,
            auth: AuthManager::new(Arc::clone(&emit)),
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
            let mut store = self.jobs.lock().expect("job store lock");
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
        let store = self.jobs.lock().expect("job store lock");
        let record = store
            .records
            .get(&request.job_id)
            .ok_or_else(|| JobError::UnknownJob(request.job_id.clone()))?;
        Ok(record.snapshot(request.include_result))
    }

    pub(crate) fn list(&self, request: RuntimeJobListRequest) -> Vec<RuntimeJobSnapshot> {
        let limit = request.limit.min(MAX_LIST_LIMIT);
        let store = self.jobs.lock().expect("job store lock");
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
            let mut store = self.jobs.lock().expect("job store lock");
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
        let outcome = if matches!(command, RuntimeCommand::AgentRun { .. }) {
            self.run_agent_command(&job_id, command, &token)
        } else if is_session_command(&command) {
            self.sessions.handle_command(command, &token)
        } else if is_auth_command(&command) {
            self.auth.handle_command(command, &token)
        } else {
            self.runtime.handle_command_cancellable(command, &token)
        };
        let event = {
            let mut store = self.jobs.lock().expect("job store lock");
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

    /// Execute an `agent.run` command: build the orchestrator (composition root
    /// concern, hence here rather than in `nerve-runtime`) and stream its agent
    /// events as `runtime/event` notifications. The job result is the run outcome.
    fn run_agent_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let RuntimeCommand::AgentRun {
            provider,
            model,
            task,
            system_prompt,
            max_turns,
            temperature,
            reasoning_effort,
            tool_filter,
        } = command
        else {
            return Err(nerve_runtime::RuntimeError::adapter(
                "expected agent.run command",
            ));
        };
        let config = agent::AgentRunConfig {
            workspace: None,
            provider,
            model,
            task,
            system_prompt,
            max_turns,
            temperature,
            reasoning_effort,
            tool_filter,
            api_key: None,
            distill_memory: false,
            verify_completion: false,
        };
        let emit = Arc::clone(&self.emit);
        let job_id = job_id.to_string();
        let mut sink = move |event: AgentEvent| {
            if let Some(runtime_event) = map_agent_event(&job_id, event) {
                emit(runtime_event);
            }
        };
        // Daemon is non-interactive: deny on `Ask` (safe default). A real
        // approval round-trip over the protocol is future Session-layer work.
        let gate = ToolGate::deny(self.policy.clone());
        match agent::run_agent(
            Arc::clone(&self.runtime),
            config,
            &self.registry,
            gate,
            token,
            &mut sink,
            self.session_store.as_ref(),
        ) {
            Ok(outcome) => Ok(json!({
                "reason": outcome.reason,
                "turns": outcome.turns,
                "final_text": outcome.final_text,
                "usage": {
                    "input_tokens": outcome.usage.input_tokens,
                    "output_tokens": outcome.usage.output_tokens,
                },
            })),
            Err(_) if token.is_cancelled() => Err(nerve_runtime::RuntimeError::cancelled()),
            Err(err) => Err(nerve_runtime::RuntimeError::adapter(err.to_string())),
        }
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

fn is_session_command(command: &RuntimeCommand) -> bool {
    matches!(
        command,
        RuntimeCommand::SessionStart { .. }
            | RuntimeCommand::SessionMessage { .. }
            | RuntimeCommand::SessionInterrupt { .. }
            | RuntimeCommand::SessionRespond { .. }
            | RuntimeCommand::SessionGet { .. }
            | RuntimeCommand::SessionList
            | RuntimeCommand::SessionClose { .. }
            | RuntimeCommand::SessionSetModel { .. }
    )
}

fn is_auth_command(command: &RuntimeCommand) -> bool {
    matches!(
        command,
        RuntimeCommand::AuthStart { .. }
            | RuntimeCommand::AuthComplete { .. }
            | RuntimeCommand::AuthStatus { .. }
            | RuntimeCommand::AuthLogout { .. }
    )
}

fn map_agent_event(job_id: &str, event: AgentEvent) -> Option<RuntimeEvent> {
    crate::agent_event::agent_event_kind(event)
        .map(|kind| RuntimeEvent::agent(job_id.to_string(), kind))
}

fn now_ms() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}
