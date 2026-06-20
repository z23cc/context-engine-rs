//! Daemon-adapter job lifecycle state.
//!
//! `nerve-runtime` owns the Rust schema for the protocol types and method contract. This module owns
//! only the runtime daemon's local lifecycle mechanics: in-memory retention,
//! thread spawning, event emission wiring, and cooperative cancellation tokens.

use crate::auth::AuthManager;
use crate::delegate_live::LiveSessions;
use crate::delegate_runtime::{self, DelegateAgent, DelegateError, DelegateParser};
use crate::delegate_session::DelegateSession;
use crate::policy::{Policy, ToolGate};
use crate::sandbox::SandboxLauncher;
use crate::session::SessionStore;
use crate::session_manager::SessionManager;
use crate::{agent, providers::ProviderRegistry, tools};
use nerve_agent::AgentEvent;
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{
    DelegateAutonomy, RuntimeCommand, RuntimeEvent, RuntimeJobError, RuntimeJobGetRequest,
    RuntimeJobListRequest, RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
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
    /// Containment backend for `delegate.start` jobs, bound to the **trust
    /// context** like [`agent::AgentRunConfig::exec_launcher`]: a refusing
    /// launcher unless the daemon was started with `--allow-delegate`, so a
    /// served daemon refuses to spawn external agents by default.
    delegate_launcher: Arc<dyn SandboxLauncher>,
    /// Live, steerable delegated sessions (DA-5a), keyed by their `delegate.start`
    /// job id. A `claude` start job registers its session here and parks; later
    /// `delegate.steer` / `delegate.close` commands route through this registry.
    live_sessions: LiveSessions,
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
            live_sessions: LiveSessions::default(),
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
            Executor::Session => self.sessions.handle_command(command, &token),
            Executor::Auth => self.auth.handle_command(command, &token),
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
            // Daemon-served runs refuse exec by trust context, not just by flag.
            allow_exec: false,
            exec_launcher: crate::sandbox::refuse_launcher(),
            // One-shot daemon `agent.run` jobs don't expose the delegate tool;
            // delegation is the dedicated `delegate.start` job (DA-2) and the
            // session chat-tool path (DA-3). Refuse by trust context here.
            allow_delegate: false,
            delegate_launcher: crate::sandbox::refuse_launcher(),
            delegate_event_sink: None,
            // One-shot agent.run jobs start fresh (resume is the session layer).
            resume_truncations: 0,
            // Cost budget guard is opt-in; off for daemon agent.run jobs.
            cost_budget_usd: None,
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

    /// Execute a `delegate.*` command. `delegate.start` (DA-2/DA-5a) resolves the
    /// agent and either runs a one-shot CLI (codex/gemini) or starts a live,
    /// steerable `claude` session that parks for later steering; `delegate.steer`
    /// and `delegate.close` (DA-5a) route into the live-session registry by
    /// session id. A refusing `delegate_launcher` (the default trust context)
    /// surfaces a clear "delegation is disabled" error instead of spawning.
    fn run_delegate_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::DelegateStart {
                agent,
                task,
                cwd,
                autonomy,
                model,
            } => self.run_delegate_start(job_id, &agent, &task, cwd, autonomy, model, token),
            RuntimeCommand::DelegateSteer {
                session_id,
                message,
            } => self.run_delegate_steer(&session_id, &message, token),
            RuntimeCommand::DelegateClose { session_id } => self.run_delegate_close(&session_id),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a delegate.* command",
            )),
        }
    }

    /// Start a delegated run: a `claude` agent becomes a live, steerable session
    /// (DA-5a) that parks after turn 1; codex/gemini stay one-shot (DA-2).
    #[allow(clippy::too_many_arguments)] // reason: one cohesive start call; the agent
    // name, task, cwd, autonomy, and model are independent inputs to the spawn,
    // and bundling them into a struct would add indirection without isolating any
    // separate responsibility.
    fn run_delegate_start(
        &self,
        job_id: &str,
        agent: &str,
        task: &str,
        cwd: Option<String>,
        autonomy: DelegateAutonomy,
        model: Option<String>,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let resolved = DelegateAgent::from_name(agent)
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        let root = self.delegate_root()?;
        let run_cwd = delegate_runtime::resolve_delegate_cwd(&root, cwd.as_deref())
            .map_err(delegate_error)?;
        if resolved == DelegateAgent::Claude {
            return self.run_delegate_live(job_id, &run_cwd, autonomy, task, model, token);
        }
        self.run_delegate(
            job_id, resolved, agent, task, &run_cwd, autonomy, model, token,
        )
    }

    /// Start a live `claude` session: spawn the persistent child, run turn 1
    /// (streaming progress), register the session, and park the job thread until
    /// close/cancel. The job stays `running` while parked, so a client can steer
    /// it. The job result (delivered when it finally closes) is turn 1's outcome.
    fn run_delegate_live(
        &self,
        job_id: &str,
        cwd: &std::path::Path,
        autonomy: DelegateAutonomy,
        task: &str,
        model: Option<String>,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let emit = Arc::clone(&self.emit);
        let job = job_id.to_string();
        let mut on_progress = |text: &str| {
            emit(RuntimeEvent::delegate_progress(
                job.clone(),
                "claude",
                text.to_string(),
            ));
        };
        let (session, turn) = DelegateSession::start(
            self.delegate_launcher.as_ref(),
            cwd,
            autonomy,
            model.as_deref(),
            task,
            token,
            &mut on_progress,
        )
        .map_err(|err| delegate_session_error("claude", &err.to_string()))?;
        if token.is_cancelled() {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        let session_id = session.session_id().unwrap_or(job_id).to_string();
        let result = turn.to_json(&session_id);
        // Register and park: the session stays live for `delegate.steer` until a
        // `delegate.close` (or cancel) wakes this thread to tear it down.
        let handle = self.live_sessions.register(job_id, session);
        self.live_sessions.park_until_closed(job_id, &handle);
        if token.is_cancelled() {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        Ok(result)
    }

    /// Steer a live delegated session with a follow-up message, streaming this
    /// turn's progress and returning the turn result. Errors if no live session is
    /// registered under `session_id`.
    fn run_delegate_steer(
        &self,
        session_id: &str,
        message: &str,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let handle = self
            .live_sessions
            .get(session_id)
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        let emit = Arc::clone(&self.emit);
        let session = session_id.to_string();
        let mut on_progress = |text: &str| {
            emit(RuntimeEvent::delegate_progress(
                session.clone(),
                "claude",
                text.to_string(),
            ));
        };
        let turn = handle
            .steer(message, token, &mut on_progress)
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        if token.is_cancelled() {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        Ok(turn.to_json(session_id))
    }

    /// Close a live delegated session: request close (the parked start thread then
    /// reaps the child and deregisters). Errors if the id is unknown.
    fn run_delegate_close(&self, session_id: &str) -> Result<Value, nerve_runtime::RuntimeError> {
        self.live_sessions
            .close(session_id)
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        Ok(json!({ "session_id": session_id, "closed": true }))
    }

    /// Resolve the workspace root a delegated run is confined to (the default
    /// workspace's first root). Delegation needs a concrete root to confine `cwd`.
    fn delegate_root(&self) -> Result<std::path::PathBuf, nerve_runtime::RuntimeError> {
        self.runtime
            .resolver()
            .resolve_workspace(None)
            .ok()
            .and_then(|workspace| workspace.roots().first().map(|root| root.path.clone()))
            .ok_or_else(|| {
                nerve_runtime::RuntimeError::adapter(
                    "delegation requires a served workspace root (start the daemon with --root)",
                )
            })
    }

    /// Spawn the delegated CLI through the streaming launcher, forwarding progress
    /// and parsing the stream into a [`DelegateOutcome`].
    #[allow(clippy::too_many_arguments)] // reason: one cohesive spawn call; splitting the
    // resolved/raw agent name, cwd, autonomy, and model into a struct would add
    // indirection without separating any responsibility.
    fn run_delegate(
        &self,
        job_id: &str,
        resolved: DelegateAgent,
        agent_name: &str,
        task: &str,
        cwd: &std::path::Path,
        autonomy: DelegateAutonomy,
        model: Option<String>,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let invocation =
            delegate_runtime::build_command(resolved, task, cwd, autonomy, model.as_deref());
        let policy = delegate_runtime::delegate_policy(cwd);
        let mut parser = DelegateParser::new(resolved);
        let emit = Arc::clone(&self.emit);
        let job = job_id.to_string();
        let agent_owned = agent_name.to_string();
        let mut on_line = |line: &str| {
            if let Some(text) = parser.ingest(line) {
                emit(RuntimeEvent::delegate_progress(
                    job.clone(),
                    agent_owned.clone(),
                    text,
                ));
            }
        };
        let output = self
            .delegate_launcher
            .launch_streaming(
                &invocation.spec,
                &policy,
                &invocation.stdin,
                token,
                &mut on_line,
            )
            .map_err(|err| delegate_launch_error(agent_name, &err))?;
        if token.is_cancelled() {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        let outcome = parser.finish(agent_name, output.exit_code, output.timed_out);
        Ok(outcome.to_json())
    }

    fn emit(&self, event: RuntimeEvent) {
        (self.emit)(event);
    }
}

/// Map a delegate-runtime caller error (unknown agent / cwd escape) to a protocol
/// adapter error.
fn delegate_error(err: DelegateError) -> nerve_runtime::RuntimeError {
    nerve_runtime::RuntimeError::adapter(err.to_string())
}

/// Render a launcher failure for a delegated spawn. A refusing launcher (the
/// default daemon trust context) produces a message that points at the lift flag;
/// any other spawn failure is surfaced verbatim.
fn delegate_launch_error(agent: &str, err: &anyhow::Error) -> nerve_runtime::RuntimeError {
    let message = err.to_string();
    if message.contains("no contained sandbox backend") {
        return nerve_runtime::RuntimeError::adapter(format!(
            "delegation is disabled: cannot spawn agent `{agent}` (start the daemon with \
             --allow-delegate, which requires a non-refusing exec context)"
        ));
    }
    nerve_runtime::RuntimeError::adapter(format!("delegate `{agent}` failed: {message}"))
}

/// Render a live-session start failure (DA-5a). Like [`delegate_launch_error`], a
/// refused persistent spawn (the default trust context) points at the lift flag;
/// any other failure (the child died, a turn stalled) is surfaced verbatim.
fn delegate_session_error(agent: &str, message: &str) -> nerve_runtime::RuntimeError {
    if message.contains("no contained sandbox backend") {
        return nerve_runtime::RuntimeError::adapter(format!(
            "delegation is disabled: cannot start a live `{agent}` session (start the daemon with \
             --allow-delegate, which requires a non-refusing exec context)"
        ));
    }
    nerve_runtime::RuntimeError::adapter(format!("delegate `{agent}` session failed: {message}"))
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
    /// The host `SessionManager` (`session.*` family).
    Session,
    /// The host `AuthManager` (`auth.*` family).
    Auth,
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
        | RuntimeCommand::DelegateClose { .. } => Executor::Delegate,
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
        | RuntimeCommand::AuthLogout { .. } => Executor::Auth,
        RuntimeCommand::Ping | RuntimeCommand::ToolList | RuntimeCommand::ToolCall { .. } => {
            Executor::CoreHub
        }
    }
}

fn map_agent_event(job_id: &str, event: AgentEvent) -> Option<RuntimeEvent> {
    // Streaming tool-call fragments map to the job-scoped `ToolCallDelta`
    // RuntimeEvent (advisory/UI-only) rather than a structured agent step.
    if let AgentEvent::ToolCallDelta { name, arguments } = &event {
        let delta = tool_call_delta_payload(name, arguments);
        return Some(RuntimeEvent::tool_call_delta(
            job_id.to_string(),
            delta,
            None,
        ));
    }
    crate::agent_event::agent_event_kind(event)
        .map(|kind| RuntimeEvent::agent(job_id.to_string(), kind))
}

/// Render an advisory tool-call delta as a compact `name(arguments)` string for
/// the UI-only `ToolCallDelta` event. The wire shape carries a raw delta string.
fn tool_call_delta_payload(name: &str, arguments: &serde_json::Value) -> String {
    format!("{name}({arguments})")
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
    use std::collections::BTreeSet;

    /// Build a minimal representative value for a protocol command name by the
    /// real `kind`-tagged deserialization path (so a `kind` rename that drifts
    /// from `RUNTIME_COMMAND_NAMES` is caught too). Panics on an unknown name, so
    /// adding a name without teaching this test fails loudly — which then forces
    /// an executor decision in `every_runtime_command_has_exactly_one_executor`.
    fn representative(name: &str) -> RuntimeCommand {
        let fields: Value = match name {
            "ping" | "tool.list" | "session.list" => json!({}),
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
            "auth.start" | "auth.status" | "auth.logout" => json!({ "provider": "p" }),
            "auth.complete" => json!({ "login_id": "l" }),
            "delegate.start" => json!({ "agent": "codex", "task": "t" }),
            "delegate.steer" => json!({ "session_id": "s", "message": "m" }),
            "delegate.close" => json!({ "session_id": "s" }),
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
            Executor::Session,
            Executor::Auth,
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
