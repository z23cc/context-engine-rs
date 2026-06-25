//! Daemon-adapter job lifecycle state.
//!
//! `nerve-runtime` owns the Rust schema for the protocol types and method contract. This module owns
//! only the runtime daemon's local lifecycle mechanics: in-memory retention,
//! thread spawning, event emission wiring, and cooperative cancellation tokens.

use crate::auth::AuthManager;
use crate::delegate_live::LiveSessions;
use crate::delegate_proxy::{DelegateDecisions, DelegateProxy};
use crate::delegate_runtime::{self, DelegateAgent, DelegateError, DelegateParser};
use crate::delegate_session::DelegateSession;
use crate::flow_job::{self, FlowDeps, LiveFlows};
use crate::policy::{Policy, ToolGate};
use crate::sandbox::SandboxLauncher;
use crate::session::SessionStore;
use crate::session_manager::SessionManager;
use crate::{agent, providers::ProviderRegistry, tools};
use nerve_agent::AgentEvent;
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{
    DelegateAutonomy, DelegateRole, FlowSource, HostCapabilities, HostCapabilitySupport,
    RuntimeCommand, RuntimeEvent, RuntimeJobError, RuntimeJobErrorExt, RuntimeJobGetRequest,
    RuntimeJobListRequest, RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
    SessionApprovalDecision,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_JOB_ID_LEN: usize = 128;
const MAX_HOST_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_NOTIFICATION_TITLE_BYTES: usize = 160;
const MAX_NOTIFICATION_BODY_BYTES: usize = 2 * 1024;
const MAX_DIALOG_TITLE_BYTES: usize = 160;
const MAX_DIALOG_NAME_BYTES: usize = 160;
const MAX_URL_BYTES: usize = 4096;
const WINDOWS_FOLDER_PICKER_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms;
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog;
$dialog.Description = $args[0];
$dialog.ShowNewFolderButton = $true;
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    [Console]::Out.WriteLine($dialog.SelectedPath);
    exit 0;
}
exit 2;
"#;
const WINDOWS_SAVE_FILE_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms;
$dialog = New-Object System.Windows.Forms.SaveFileDialog;
$dialog.Title = $args[0];
$dialog.FileName = $args[1];
$dialog.Filter = "Markdown files (*.md)|*.md|Text files (*.txt)|*.txt|All files (*.*)|*.*";
$dialog.OverwritePrompt = $true;
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    [Console]::Out.WriteLine($dialog.FileName);
    exit 0;
}
exit 2;
"#;
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
                role,
                model,
                mcp_enable,
            } => self.run_delegate_start(
                job_id, &agent, &task, cwd, autonomy, role, model, mcp_enable, token,
            ),
            RuntimeCommand::DelegateSteer {
                session_id,
                message,
            } => self.run_delegate_steer(&session_id, &message, token),
            RuntimeCommand::DelegateClose { session_id } => self.run_delegate_close(&session_id),
            RuntimeCommand::DelegateList => Ok(crate::delegate_store::run_delegate_list(
                &self.live_sessions,
                self.delegate_store().as_ref(),
            )),
            RuntimeCommand::DelegateGet { session_id } => crate::delegate_store::run_delegate_get(
                &session_id,
                &self.live_sessions,
                self.delegate_store().as_ref(),
            ),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a delegate.* command",
            )),
        }
    }

    /// Execute a `run.*` command (L0 flight-recorder, read-only): enumerate or fetch
    /// captured Runs from the persisted [`RunStore`](crate::run_store) for the served
    /// root. No served root => an empty list / not-found, mirroring `delegate.*`.
    fn run_run_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::RunList => {
                Ok(crate::run_store::run_run_list(self.run_store().as_ref()))
            }
            RuntimeCommand::RunGet { run_id } => {
                crate::run_store::run_run_get(&run_id, self.run_store().as_ref())
            }
            RuntimeCommand::OtelIngest { source, .. } => {
                let local = match source {
                    nerve_runtime::OtelSource::Inline { trace } => {
                        crate::otel_ingest::OtelSource::Inline { trace }
                    }
                    nerve_runtime::OtelSource::Path { trace_path } => {
                        crate::otel_ingest::OtelSource::Path { trace_path }
                    }
                };
                crate::otel_ingest::handle_otel_ingest(
                    &local,
                    self.run_store().as_ref(),
                    self.delegate_root().ok().as_deref(),
                )
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a run.* command",
            )),
        }
    }

    /// L0c — `replay.start`: re-drive a captured Run's tape and verify its content
    /// address (handler in `crate::replay`).
    fn run_replay_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::ReplayStart { run_id } => {
                let emit = |event: RuntimeEvent| self.emit(event);
                crate::replay::handle_replay_start(
                    &run_id,
                    job_id,
                    self.run_store().as_ref(),
                    &emit,
                    token,
                )
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected replay.* command",
            )),
        }
    }

    /// L1 — `ledger.query`: read the append-only evidence ledger (handler in
    /// `crate::ledger_store`).
    fn run_ledger_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::LedgerQuery {
                run_id,
                agent,
                diff_hash,
                outcome,
                record_kind,
                limit,
            } => Ok(crate::ledger_store::run_ledger_query(
                self.ledger_store().as_ref(),
                run_id.as_deref(),
                agent.as_deref(),
                diff_hash.as_deref(),
                outcome,
                record_kind.as_deref(),
                limit.unwrap_or(200),
            )),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected ledger.* command",
            )),
        }
    }

    /// L2 — `verify.*`: re-run the org's checks in the closure and seal/fetch the
    /// borrowed verdict (handlers in `crate::verify_runner`). On a fresh verify it
    /// also announces `VerificationCompleted` and appends the verdict to the ledger.
    fn run_verify_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::VerifyStart {
                run_id,
                reruns,
                only,
            } => {
                let root = self.delegate_root()?;
                let verdict = crate::verify_runner::handle_verify_start(
                    self.run_store().as_ref(),
                    self.verify_store().as_ref(),
                    &self.delegate_launcher,
                    &root,
                    &run_id,
                    reruns,
                    only.as_deref(),
                    token,
                    now_ms(),
                )?;
                self.emit(RuntimeEvent::VerificationCompleted {
                    run_id: verdict.run_id.clone(),
                    verdict_id: verdict.verdict_id.clone(),
                    status: verdict.status,
                    check_count: verdict.checks.len() as u64,
                });
                crate::ledger_store::append_evidence(
                    self.ledger_store().as_ref(),
                    nerve_core::ledger::LedgerKind::Verdict {
                        run_id: verdict.run_id.clone(),
                        diff_hash: verdict.diff_hash.clone(),
                        verdict: verdict.status,
                        checks: verdict.checks.clone(),
                        advisory_llm_judge: None,
                    },
                );
                self.issue_receipt_for_verdict(&run_id, &verdict);
                serde_json::to_value(&verdict)
                    .map(|verdict| json!({ "verdict": verdict }))
                    .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))
            }
            RuntimeCommand::VerifyGet { verdict_id } => {
                crate::verify_runner::handle_verify_get(&verdict_id, self.verify_store().as_ref())
            }
            RuntimeCommand::VerifyList { run_id } => Ok(crate::verify_runner::handle_verify_list(
                self.verify_store().as_ref(),
                run_id.as_deref(),
            )),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected verify.* command",
            )),
        }
    }

    /// L3 — `policy.*`: serve the sealed policy doc + decision evidence (handlers in
    /// `crate::policy_plane`).
    fn run_policy_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let plane = self.policy_plane();
        match command {
            RuntimeCommand::PolicyGet => Ok(crate::policy_plane::run_policy_get(plane.as_ref())),
            RuntimeCommand::PolicyDecisions { session_id } => Ok(
                crate::policy_plane::run_policy_decisions(session_id.as_deref(), plane.as_ref()),
            ),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected policy.* command",
            )),
        }
    }

    /// L4 — `receipt.get`: fetch a signed Verification Receipt (handler in
    /// `crate::receipt_store`).
    fn run_receipt_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::ReceiptGet { receipt_id } => {
                crate::receipt_store::run_receipt_get(&receipt_id, self.receipt_store().as_ref())
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected receipt.* command",
            )),
        }
    }

    /// L6 — `outcome.*`: append/get/query human/CI outcome labels (handlers in
    /// `crate::outcome_store`); a label append announces `OutcomeLabeled`.
    fn run_outcome_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::OutcomeLabel {
                run_id,
                outcome,
                source,
                actor,
                note,
                verdict_ref,
            } => {
                let (payload, run_id, labels_root, label_count) =
                    crate::outcome_store::handle_outcome_label(
                        &run_id,
                        outcome,
                        source,
                        actor,
                        note,
                        verdict_ref,
                        self.outcome_store().as_ref(),
                    )?;
                self.emit(RuntimeEvent::OutcomeLabeled {
                    run_id,
                    session_id: None,
                    outcome,
                    labels_root,
                    label_count,
                });
                Ok(payload)
            }
            RuntimeCommand::OutcomeGet { run_id } => {
                crate::outcome_store::handle_outcome_get(&run_id, self.outcome_store().as_ref())
            }
            RuntimeCommand::OutcomeQuery {
                agent,
                outcome,
                limit,
            } => Ok(crate::outcome_store::handle_outcome_query(
                agent.as_deref(),
                outcome,
                limit.unwrap_or(200),
                self.outcome_store().as_ref(),
            )),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected outcome.* command",
            )),
        }
    }

    /// L1 evidence ledger store for the served root (mirrors `run_store`).
    fn ledger_store(&self) -> Option<crate::ledger_store::LedgerStore> {
        crate::ledger_store::LedgerStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L2 verdict store for the served root.
    fn verify_store(&self) -> Option<crate::verify_store::VerifyStore> {
        crate::verify_store::VerifyStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L4 receipt store for the served root.
    fn receipt_store(&self) -> Option<crate::receipt_store::ReceiptStore> {
        crate::receipt_store::ReceiptStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L6 outcome corpus store for the served root.
    fn outcome_store(&self) -> Option<crate::outcome_store::OutcomeStore> {
        crate::outcome_store::OutcomeStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L3 policy plane for the served root (sealed policy doc + a null evidence sink;
    /// the ledger-backed sink is wired when L3↔L1 is promoted).
    fn policy_plane(&self) -> Option<crate::policy_plane::PolicyPlane> {
        Some(crate::policy_plane::PolicyPlane::resolve(
            self.delegate_root().ok().as_deref(),
        ))
    }

    /// L4 — issue a signed Verification Receipt binding a sealed verdict (best-effort),
    /// so `receipt.get` returns a portable, offline-re-verifiable attestation. Reloads
    /// the run, maps the verdict's per-check results, signs via the local ed25519
    /// signer, persists, and announces `ReceiptIssued`. A missing run/store is a silent
    /// no-op — issuing a receipt never fails the verify turn.
    fn issue_receipt_for_verdict(&self, run_id: &str, verdict: &nerve_core::verdict::Verdict) {
        let Some(run) = self
            .run_store()
            .and_then(|store| store.load_record(run_id).ok())
        else {
            return;
        };
        let checks: Vec<nerve_core::receipt::ReceiptCheck> = verdict
            .checks
            .iter()
            .map(|check| nerve_core::receipt::ReceiptCheck {
                name: check.name.clone(),
                kind: check.kind,
                verdict: check_status_to_verdict(check.status),
                reproducible: check.reproducible,
                evidence_hash: None,
            })
            .collect();
        let toolchain_digest =
            (!run.inputs.toolchain_digest.is_empty()).then(|| run.inputs.toolchain_digest.clone());
        if let Some(issued) = crate::receipt_store::issue_receipt_for_run(
            &run,
            checks,
            // Borrow the sealed L2 verdict (honours the `required` mask) — never
            // re-derive a stricter/looser one from the evidence checks (INV-R1).
            verdict.status,
            toolchain_digest,
            None,
            None,
            now_ms(),
            &self.signer(),
            self.receipt_store().as_ref(),
        ) {
            self.emit(RuntimeEvent::ReceiptIssued {
                session_id: run.session_id.clone(),
                run_id: issued.run_id,
                receipt_id: issued.receipt_id,
                verdict: issued.verdict,
            });
        }
    }

    /// The local ed25519 receipt signer, keyed under `config_home()/keys` (stable
    /// across projects), falling back to the served root's `.nerve/keys`.
    fn signer(&self) -> crate::signer::LocalEd25519Signer {
        let dir = nerve_agent::auth::config_home()
            .map(|home| home.join("keys"))
            .ok()
            .or_else(|| {
                self.delegate_root()
                    .ok()
                    .map(|root| root.join(".nerve").join("keys"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from(".nerve/keys"));
        crate::signer::LocalEd25519Signer::load_or_create(&dir)
    }

    /// Execute a host-side command. These commands are the declared runtime seam
    /// for OS/native shell capabilities; the pure core runtime deliberately does
    /// not know about windows, menus, pasteboards, or process launchers.
    fn run_host_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::HostCapabilities => host_capabilities_value(),
            RuntimeCommand::HostClipboardWriteText { text } => run_clipboard_write_text(&text),
            RuntimeCommand::HostNotificationShow { title, body } => {
                run_notification_show(&title, body.as_deref())
            }
            RuntimeCommand::HostFolderPick { title } => run_folder_pick(title.as_deref()),
            RuntimeCommand::HostFileSaveText {
                title,
                default_name,
                text,
            } => run_file_save_text(title.as_deref(), default_name.as_deref(), &text, token),
            RuntimeCommand::HostUrlOpen { url } => run_url_open(&url),
            RuntimeCommand::WorkspaceReveal { workspace } => {
                self.run_workspace_reveal(workspace.as_deref())
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a host.* or workspace.* command",
            )),
        }
    }

    /// Execute a `wechat.*` command, delegating to the daemon-hosted [`WechatHost`]
    /// (`crate::wechat`). `wechat.login` runs the cancellable QR flow on this job
    /// thread; `wechat.start` requires `--allow-delegate` + a served `--root` (it
    /// drives delegated agents) and spawns the bridge on its own thread; `stop` /
    /// `status` are immediate.
    fn run_wechat_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::WechatLogin { bot_type, base_url } => {
                self.wechat.login(&bot_type, base_url.as_deref(), token)
            }
            RuntimeCommand::WechatStart {
                owners,
                agent,
                autonomy,
            } => {
                if !self.allow_delegate {
                    return Err(nerve_runtime::RuntimeError::adapter(
                        "the WeChat bridge drives delegated agents — start the daemon with \
                         --allow-delegate",
                    ));
                }
                let root = self.delegate_root()?;
                self.wechat.start(
                    Arc::clone(&self.delegate_launcher),
                    root,
                    owners,
                    agent,
                    autonomy,
                )
            }
            RuntimeCommand::WechatStop => self.wechat.stop(),
            RuntimeCommand::WechatStatus => Ok(self.wechat.status()),
            other => Err(nerve_runtime::RuntimeError::adapter(format!(
                "expected a wechat.* command, got {}",
                other.name()
            ))),
        }
    }

    /// Execute a `flow.*` command (C2). `flow.start` runs the deterministic C1 flow
    /// engine as ONE cancellable job (the `job_id` IS the `flow_id`), mapping the
    /// engine's worker events + node lifecycle onto the `flow_*` runtime events;
    /// `flow.get` / `flow.list` / `flow.close` route through the live-flow registry;
    /// `flow.respond` resolves a pending approval keyed by `flow_id` through the SAME
    /// [`ApprovalHub`](crate::session_manager::ApprovalHub) `session.respond` uses.
    fn run_flow_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::FlowStart { workflow, .. } => {
                self.run_flow_start(job_id, workflow, token)
            }
            RuntimeCommand::FlowSteer {
                flow_id,
                target,
                message,
            } => self.run_flow_steer(&flow_id, &target, &message, token),
            RuntimeCommand::FlowReplay { ledger_ref, .. } => {
                self.run_flow_replay(job_id, ledger_ref, token)
            }
            RuntimeCommand::FlowGet { flow_id } => {
                let store =
                    crate::flow_store::FlowStore::for_scope(self.workspace_root().as_deref()).ok();
                flow_job::run_flow_get(&flow_id, &self.flows, store.as_ref())
            }
            RuntimeCommand::FlowList => {
                let store =
                    crate::flow_store::FlowStore::for_scope(self.workspace_root().as_deref()).ok();
                flow_job::run_flow_list(&self.flows, store.as_ref())
            }
            RuntimeCommand::FlowClose { flow_id } => {
                flow_job::run_flow_close(&flow_id, &self.flows)
            }
            RuntimeCommand::FlowRespond {
                flow_id,
                request_id,
                decision,
            } => self.run_flow_respond(&flow_id, &request_id, decision),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a flow.* command",
            )),
        }
    }

    /// Run a `flow.start` as a job: build the shared [`FlowDeps`] from the daemon's
    /// own runtime / registry / policy / launcher / approval hub, then drive the
    /// flow engine to completion ([`flow_job::run_flow_start`]). The `job_id` is the
    /// `flow_id`. CLI worker nodes require the `--allow-delegate` lift; provider
    /// worker nodes do not (mirrors the C1 `FlowArgs` gating).
    fn run_flow_start(
        &self,
        job_id: &str,
        workflow: FlowSource,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let deps = self.flow_deps();
        flow_job::run_flow_start(
            job_id,
            workflow,
            &deps,
            &self.flows,
            &self.emit,
            self.allow_delegate,
            token,
        )
    }

    /// Resolve a `flow.respond` through the shared approval hub keyed by `flow_id`.
    fn run_flow_respond(
        &self,
        flow_id: &str,
        request_id: &str,
        decision: SessionApprovalDecision,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        flow_job::run_flow_respond(flow_id, request_id, decision, &self.sessions.approvals())
    }

    /// Steer a live flow branch (C3a): look up the running flow's live-flow worker
    /// registry and run one more turn against the branch `target` selects, streaming
    /// the follow-up as `flow_node_agent` events scoped to `flow_id`. Runs as its own
    /// short job (`token` is that job's cancel) — distinct from the `flow.start` job.
    fn run_flow_steer(
        &self,
        flow_id: &str,
        target: &nerve_runtime::WorkerSelector,
        message: &str,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        flow_job::run_flow_steer(flow_id, target, message, &self.flows, &self.emit, token)
    }

    /// Assemble the [`FlowDeps`] the flow engine needs, all cloned from the daemon's
    /// own deps so a flow reaches tools through the SAME runtime, is gated by the
    /// SAME policy, and routes approvals through the SAME hub as sessions/delegation.
    /// The [`FlowStore`] is resolved per-call from the workspace root so a flow's tape
    /// persists under that project's `.nerve/flows` (C4); a rootless/unresolvable
    /// scope falls back to the global flows dir, and a resolve error disables
    /// persistence rather than failing the flow.
    fn flow_deps(&self) -> FlowDeps {
        let root = self.workspace_root();
        FlowDeps {
            runtime: Arc::clone(&self.runtime),
            registry: self.registry.clone(),
            policy: self.policy.clone(),
            delegate_launcher: Arc::clone(&self.delegate_launcher),
            approvals: self.sessions.approvals(),
            store: crate::flow_store::FlowStore::for_scope(root.as_deref()).ok(),
        }
    }

    /// The workspace root the daemon is scoped to (the first registered root), used
    /// to locate `.nerve/flows`. `None` when no root resolves.
    fn workspace_root(&self) -> Option<std::path::PathBuf> {
        self.runtime
            .resolver()
            .resolve_workspace(None)
            .ok()
            .and_then(|ws| ws.roots().first().map(|r| r.path.clone()))
    }

    /// Run a `flow.replay` as a job (C4, design §3/§4): load the recorded ledger +
    /// def from the [`FlowStore`] and re-run the engine in REPLAY mode, re-emitting the
    /// `flow_*` event stream byte-identically at zero cost. The `job_id` is the
    /// replayed `flow_id`.
    fn run_flow_replay(
        &self,
        job_id: &str,
        ledger_ref: nerve_runtime::LedgerRef,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let deps = self.flow_deps();
        flow_job::run_flow_replay(job_id, ledger_ref, &deps, &self.flows, &self.emit, token)
    }

    /// Start a delegated run: a `claude` (DA-5a) or `codex` (DA-5c) agent becomes a
    /// live, steerable session that parks after turn 1; `gemini` stays one-shot
    /// (DA-2).
    ///
    /// DA-6: for a **codex** run, compute the `-c mcp_servers.<name>.enabled=false`
    /// flags from the effective allowlist (per-call `mcp_enable` if present, else the
    /// persisted `[delegate.codex] mcp_enable` config) and thread them into the argv;
    /// claude/gemini get an empty set and are unaffected.
    #[allow(clippy::too_many_arguments)] // reason: one cohesive start call; the agent
    // name, task, cwd, autonomy, model, and per-call mcp allowlist are independent
    // inputs to the spawn, and bundling them into a struct would add indirection
    // without isolating any separate responsibility.
    fn run_delegate_start(
        &self,
        job_id: &str,
        agent: &str,
        task: &str,
        cwd: Option<String>,
        autonomy: DelegateAutonomy,
        role: DelegateRole,
        model: Option<String>,
        mcp_enable: Option<Vec<String>>,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        // DA-7: expand the role preset once, before the live/one-shot split, so both
        // downstream paths inherit the scout's wrapped task + forced read-only posture.
        let (task, autonomy) = crate::delegate_roles::apply_role(role, task, autonomy);
        let task = task.as_str();
        let resolved = DelegateAgent::from_name(agent)
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        let root = self.delegate_root()?;
        let run_cwd = delegate_runtime::resolve_delegate_cwd(&root, cwd.as_deref())
            .map_err(delegate_error)?;
        let mcp_disable_flags =
            crate::delegate_codex_mcp::delegate_disable_flags(resolved, mcp_enable);
        if matches!(resolved, DelegateAgent::Claude | DelegateAgent::Codex) {
            // Persist a durable record (audit + resume seam, trust-substrate floor) so
            // the live session survives a daemon restart. Best-effort: a write failure
            // never blocks the start.
            if let Ok(store) = crate::delegate_store::DelegateStore::for_scope(Some(&root)) {
                let record = crate::delegate_store::DelegateSessionRecord::begin(
                    job_id,
                    resolved.catalog_name(),
                    &root,
                    &run_cwd,
                    autonomy,
                    role,
                    model.clone(),
                );
                let _ = store.write_record(&record);
            }
            return self.run_delegate_live(
                job_id,
                resolved,
                &run_cwd,
                autonomy,
                task,
                model,
                &mcp_disable_flags,
                token,
            );
        }
        self.run_delegate(
            job_id,
            resolved,
            agent,
            task,
            &run_cwd,
            autonomy,
            model,
            &mcp_disable_flags,
            token,
        )
    }

    /// Start a live session (claude or codex): spawn the persistent child, run turn
    /// 1 (streaming progress), register the live driver, and park the job thread
    /// until close/cancel. The job stays `running` while parked, so a client can
    /// steer it. The job result (delivered when it finally closes) is turn 1's
    /// outcome.
    #[allow(clippy::too_many_arguments)] // reason: one cohesive start call; the
    // resolved agent, cwd, autonomy, task, model, and codex mcp-disable flags are
    // independent spawn inputs; bundling them adds indirection without isolating a
    // responsibility.
    fn run_delegate_live(
        &self,
        job_id: &str,
        resolved: DelegateAgent,
        cwd: &std::path::Path,
        autonomy: DelegateAutonomy,
        task: &str,
        model: Option<String>,
        mcp_disable_flags: &[String],
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let agent = resolved.catalog_name();
        // L0 run capture: stamp the session start now; each turn's outcome is recorded
        // into the live handle and sealed into one content-addressed Run at close.
        let started_at_ms = now_ms();
        let emit = Arc::clone(&self.emit);
        let job = job_id.to_string();
        let mut on_progress = |text: &str| {
            emit(RuntimeEvent::delegate_progress(
                job.clone(),
                agent,
                text.to_string(),
            ));
        };
        // Proxied mode (DA-5b/5c): route the delegated agent's tool-permission
        // prompts through the SAME approval hub the SessionManager resolves
        // `session.respond` against, keyed by the delegate job id — so the TUI modal
        // and `SessionRespond` reach delegated approvals exactly as they reach
        // agent-tool approvals.
        let proxy = self.delegate_proxy(job_id, agent);
        let (mut driver, turn) = self
            .start_live_driver(
                resolved,
                cwd,
                autonomy,
                task,
                model,
                proxy,
                mcp_disable_flags,
                token,
                &mut on_progress,
            )
            // A start that fails because the job was cancelled (e.g. cancel arrived
            // while turn 1 was blocked on a tool approval) maps to `cancelled()` so
            // the job finishes as `job_cancelled`, not `job_failed`.
            .map_err(|err| {
                if token.is_cancelled() {
                    nerve_runtime::RuntimeError::cancelled()
                } else {
                    delegate_session_error(agent, &err.to_string())
                }
            })?;
        if token.is_cancelled() {
            // Cancel landed between turn-1 success and registration: the live child
            // is spawned but not yet registered/parked, so reap it here — a bare drop
            // does NOT kill the process group. Mirrors the close path's teardown.
            driver.close();
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        let session_id = driver.session_id().unwrap_or(job_id).to_string();
        // Persist the agent's OWN captured session/thread id (the resume key) now that
        // turn 1 has established it. Best-effort, keyed by the start-job id.
        let captured = driver.session_id().map(str::to_string);
        self.persist_delegate_update(job_id, |record| record.set_agent_session_id(captured));
        let result = turn.to_json(agent, &session_id);
        // Turn 1 finished; the live session is now idle (parked) and ready to
        // steer. Signal idle keyed by the JOB id (the client's delegate session
        // id) so a client can stop its turn spinner — symmetric with session.*
        // turns, which have no other turn-end event while the start job parks.
        (self.emit)(RuntimeEvent::session_idle(job_id.to_string()));
        // Register and park: the session stays live for `delegate.steer` until a
        // `delegate.close` (or cancel) wakes this thread to tear it down.
        // `register` records turn 1 for L0 capture before the handle is reachable;
        // each steer records its own turn under the session lock (see LiveHandle).
        let handle = self.live_sessions.register(job_id, driver, &turn);
        self.live_sessions.park_until_closed(job_id, &handle);
        // Session closed: seal every recorded turn into one content-addressed Run.
        let turns = handle.take_turns();
        self.seal_live_run(
            job_id,
            agent,
            task,
            cwd,
            started_at_ms,
            turns,
            !token.is_cancelled(),
        );
        if token.is_cancelled() {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        Ok(result)
    }

    /// Spawn the right persistent driver for `resolved` and run turn 1, returning the
    /// live driver wrapper plus the first turn's outcome. `mcp_disable_flags` apply to
    /// the codex driver only (DA-6); the claude driver ignores them.
    #[allow(clippy::too_many_arguments)] // reason: a single spawn call; the inputs are
    // independent and a struct would add indirection without isolating a concern.
    fn start_live_driver(
        &self,
        resolved: DelegateAgent,
        cwd: &std::path::Path,
        autonomy: DelegateAutonomy,
        task: &str,
        model: Option<String>,
        proxy: Option<DelegateProxy>,
        mcp_disable_flags: &[String],
        token: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<
        (
            crate::delegate_live::LiveDriver,
            crate::delegate_session::TurnResult,
        ),
        crate::delegate_session::SessionError,
    > {
        let launcher = self.delegate_launcher.as_ref();
        match resolved {
            DelegateAgent::Codex => {
                let (session, turn) = crate::delegate_session_codex::CodexSession::start(
                    launcher,
                    cwd,
                    autonomy,
                    model.as_deref(),
                    task,
                    proxy,
                    mcp_disable_flags,
                    token,
                    on_progress,
                )?;
                Ok((crate::delegate_live::LiveDriver::Codex(session), turn))
            }
            _ => {
                let (session, turn) = DelegateSession::start(
                    launcher,
                    cwd,
                    autonomy,
                    model.as_deref(),
                    task,
                    proxy,
                    token,
                    on_progress,
                )?;
                Ok((crate::delegate_live::LiveDriver::Claude(session), turn))
            }
        }
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
        // The progress label is the agent the live driver speaks — fixed for the
        // session's life, so read it once up front and stream this turn under it.
        let agent = handle
            .agent()
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        let emit = Arc::clone(&self.emit);
        let session = session_id.to_string();
        let mut on_progress = |text: &str| {
            emit(RuntimeEvent::delegate_progress(
                session.clone(),
                agent,
                text.to_string(),
            ));
        };
        let (turn, _agent) = handle
            .steer(message, token, &mut on_progress)
            .map_err(|err| {
                // A steer interrupted in flight (the steer job's own cancel, OR a
                // session-scoped close that interrupted this turn) tears the session
                // down; report it as a cancellation so the job finishes `job_cancelled`,
                // not `job_failed` — even when the steer job's own token didn't fire.
                if err.is_cancellation() || token.is_cancelled() {
                    nerve_runtime::RuntimeError::cancelled()
                } else {
                    nerve_runtime::RuntimeError::adapter(err.to_string())
                }
            })?;
        if token.is_cancelled() {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        // Steer turn finished; the session is idle again — signal it (keyed by the
        // session/job id) so a client can stop its turn spinner.
        (self.emit)(RuntimeEvent::session_idle(session_id.to_string()));
        self.persist_delegate_update(
            session_id,
            crate::delegate_store::DelegateSessionRecord::touch,
        );
        // The steer turn is recorded for L0 capture inside `LiveHandle::steer` (under
        // the session lock); the Run is sealed when the session closes.
        Ok(turn.to_json(agent, session_id))
    }

    /// Close a live delegated session: request close (the parked start thread then
    /// reaps the child and deregisters). Errors if the id is unknown.
    fn run_delegate_close(&self, session_id: &str) -> Result<Value, nerve_runtime::RuntimeError> {
        self.live_sessions
            .close(session_id)
            .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))?;
        self.persist_delegate_update(
            session_id,
            crate::delegate_store::DelegateSessionRecord::mark_closed,
        );
        Ok(json!({ "session_id": session_id, "closed": true }))
    }

    /// The durable delegate-session store for the served project root, if any
    /// (`None` when no root is served). Resolved per call, matching the `FlowStore`
    /// convention (no held field) — see `crate::delegate_store`.
    fn delegate_store(&self) -> Option<crate::delegate_store::DelegateStore> {
        crate::delegate_store::DelegateStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// The durable L0 run store for the served project root, if any (`None` when no
    /// root is served). Resolved per call, matching the `DelegateStore`/`FlowStore`
    /// convention (no held field) — see `crate::run_store`.
    fn run_store(&self) -> Option<crate::run_store::RunStore> {
        crate::run_store::RunStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// Best-effort update of a persisted delegate record (load -> mutate -> write): a
    /// no-op when no root is served or the record was never written, and it NEVER
    /// fails the delegated turn (persistence is an audit/resume seam, not a gate).
    fn persist_delegate_update(
        &self,
        session_id: &str,
        update: impl FnOnce(&mut crate::delegate_store::DelegateSessionRecord),
    ) {
        let Some(store) = self.delegate_store() else {
            return;
        };
        let Ok(mut record) = store.load_record(session_id) else {
            return;
        };
        update(&mut record);
        let _ = store.write_record(&record);
    }

    /// Build the proxied-mode approval bridge (DA-5b/5c) for a delegated `agent`
    /// session keyed under its start-job id. The approver is the SessionManager's
    /// `ApprovalHub` — the same hub `session.respond` resolves against — so a
    /// delegated tool prompt rides the existing approval modal + `SessionRespond`
    /// round-trip. A fresh per-session [`DelegateDecisions`] memory backs
    /// allow-always / deny-always for the life of the session. The `agent` selects
    /// the per-agent tool-tier classifier + preview label.
    fn delegate_proxy(&self, job_id: &str, agent: &str) -> Option<DelegateProxy> {
        let approver: Arc<dyn crate::delegate_proxy::DelegateApprover> = self.sessions.approvals();
        Some(DelegateProxy::for_agent(
            approver,
            job_id.to_string(),
            DelegateDecisions::default(),
            agent,
        ))
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

    /// Reveal a served workspace root in the OS file manager (`workspace.reveal`).
    /// `workspace` selects which root when more than one is registered (single-root
    /// today). Resolves the root, then spawns the platform opener (macOS `open` /
    /// Windows `explorer` / Linux `xdg-open`) — a host side-effect, kept out of the
    /// pure engine.
    fn run_workspace_reveal(
        &self,
        workspace: Option<&str>,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let root = self
            .runtime
            .resolver()
            .resolve_workspace(workspace)
            .ok()
            .and_then(|ws| ws.roots().first().map(|root| root.path.clone()))
            .ok_or_else(|| {
                nerve_runtime::RuntimeError::adapter(
                    "workspace.reveal requires a served workspace root (start the daemon with --root)",
                )
            })?;
        std::process::Command::new(workspace_opener())
            .arg(&root)
            .spawn()
            .map_err(|err| {
                nerve_runtime::RuntimeError::adapter(format!("reveal {}: {err}", root.display()))
            })?;
        Ok(json!({ "revealed": root.to_string_lossy() }))
    }

    /// Spawn the delegated CLI through the streaming launcher, forwarding progress
    /// and parsing the stream into a [`DelegateOutcome`].
    #[allow(clippy::too_many_arguments)] // reason: one cohesive spawn call; splitting the
    // resolved/raw agent name, cwd, autonomy, model, and codex mcp-disable flags into
    // a struct would add indirection without separating any responsibility.
    fn run_delegate(
        &self,
        job_id: &str,
        resolved: DelegateAgent,
        agent_name: &str,
        task: &str,
        cwd: &std::path::Path,
        autonomy: DelegateAutonomy,
        model: Option<String>,
        mcp_disable_flags: &[String],
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let invocation = delegate_runtime::build_command(
            resolved,
            task,
            cwd,
            autonomy,
            model.as_deref(),
            mcp_disable_flags,
        );
        let policy = delegate_runtime::delegate_policy(cwd);
        let mut parser = DelegateParser::new(resolved);
        let emit = Arc::clone(&self.emit);
        let job = job_id.to_string();
        let agent_owned = agent_name.to_string();
        // L0 run capture (best-effort): record the delegated run as a
        // content-addressed event tape ALONGSIDE the existing — untouched —
        // DelegateProgress stream. The raw stdout/stderr line is the tape's
        // `Output` unit; persistence happens at seal and never fails the turn.
        let root = self.delegate_root().ok().map(|p| p.display().to_string());
        let mut writer = crate::run_store::RunWriter::begin(job_id, agent_name, root);
        writer.push(nerve_core::provenance::EventKind::RunStarted {
            agent: agent_name.to_string(),
            task: task.to_string(),
            cwd: Some(cwd.display().to_string()),
            // L0c pinned inputs (repo-snapshot + toolchain digest) are wired in the
            // capture path in a follow-up; absent here, so the content address is
            // unchanged from pre-L0c (None -> skip_serialized).
            // L0c: pin the run's executed closure (repo snapshot + toolchain digest)
            // in-band so its content address commits to *what ran*, not just output.
            inputs: Some(crate::toolchain_pin::resolve_run_inputs(
                self.delegate_root().ok().as_deref(),
            )),
        });
        writer.push(nerve_core::provenance::EventKind::TurnStarted { turn: 0 });
        let launch = {
            let mut on_line = |line: &str| {
                writer.push(nerve_core::provenance::EventKind::Output {
                    turn: 0,
                    text: line.to_string(),
                });
                if let Some(text) = parser.ingest(line) {
                    emit(RuntimeEvent::delegate_progress(
                        job.clone(),
                        agent_owned.clone(),
                        text,
                    ));
                }
            };
            self.delegate_launcher.launch_streaming(
                &invocation.spec,
                &policy,
                &invocation.stdin,
                token,
                &mut on_line,
            )
        };
        let output = launch.map_err(|err| delegate_launch_error(agent_name, &err))?;
        let cancelled = token.is_cancelled();
        let outcome = parser.finish(agent_name, output.exit_code, output.timed_out);
        self.seal_delegate_run(job_id, writer, &outcome, !cancelled);
        if cancelled {
            return Err(nerve_runtime::RuntimeError::cancelled());
        }
        Ok(outcome.to_json())
    }

    /// Seal a one-shot delegated run's captured tape (best-effort) and announce it.
    /// Appends the terminal usage / turn / run events derived from the
    /// [`DelegateOutcome`](delegate_runtime::DelegateOutcome), content-addresses +
    /// persists the [`Run`](crate::run_store), and emits `RunRecorded` to the client
    /// watching the session. A persistence failure is swallowed — capture is an audit
    /// seam, never a gate on the turn.
    fn seal_delegate_run(
        &self,
        job_id: &str,
        mut writer: crate::run_store::RunWriter,
        outcome: &delegate_runtime::DelegateOutcome,
        finished: bool,
    ) {
        use nerve_core::provenance::EventKind;
        if let Some(usage) = &outcome.usage {
            writer.push(delegate_usage_event(0, usage, outcome.cost_usd));
        }
        writer.push(EventKind::TurnFinished {
            turn: 0,
            ok: outcome.ok,
        });
        writer.push(EventKind::RunFinished {
            ok: outcome.ok,
            exit_code: outcome.exit_code,
            timed_out: outcome.timed_out,
        });
        self.emit_run_recorded(job_id, writer.seal(finished, self.run_store().as_ref()));
    }

    /// Seal a live delegated session's captured turns (claude/codex) into one
    /// content-addressed Run and announce it (best-effort). Each [`CapturedTurn`]
    /// becomes `TurnStarted` → the turn's verbatim raw-output `Output` lines →
    /// optional `UsageUpdated` → `TurnFinished`, bracketed by `RunStarted` /
    /// `RunFinished` — the full raw tape, matching the one-shot path. `finished` is
    /// false when the session ended by cancellation.
    #[allow(clippy::too_many_arguments)] // reason: one cohesive seal call; the run
    // identity (job_id/agent), the start context (task/cwd/started_at_ms), the
    // captured turns, and the finished flag are independent inputs, and bundling them
    // into a struct would add indirection without isolating a separate responsibility
    // (mirrors `run_delegate_live` / `start_live_driver` in this file).
    fn seal_live_run(
        &self,
        job_id: &str,
        agent: &str,
        task: &str,
        cwd: &std::path::Path,
        started_at_ms: u64,
        turns: Vec<crate::delegate_live::CapturedTurn>,
        finished: bool,
    ) {
        use nerve_core::provenance::EventKind;
        let root = self.delegate_root().ok().map(|p| p.display().to_string());
        let mut writer = crate::run_store::RunWriter::begin_at(started_at_ms, job_id, agent, root);
        writer.push(EventKind::RunStarted {
            agent: agent.to_string(),
            task: task.to_string(),
            cwd: Some(cwd.display().to_string()),
            // L0c: pin the run's executed closure (repo snapshot + toolchain digest)
            // in-band so its content address commits to *what ran*, not just output.
            inputs: Some(crate::toolchain_pin::resolve_run_inputs(
                self.delegate_root().ok().as_deref(),
            )),
        });
        let mut last_ok = true;
        for (index, captured) in turns.iter().enumerate() {
            let turn = index as u64;
            writer.push(EventKind::TurnStarted { turn });
            // The verbatim raw tape this turn produced (the live-path analogue of the
            // one-shot path's per-line Output events) — interleaved per turn.
            for line in &captured.raw_lines {
                writer.push(EventKind::Output {
                    turn,
                    text: line.clone(),
                });
            }
            if let Some(usage) = &captured.usage {
                writer.push(delegate_usage_event(turn, usage, captured.cost_usd));
            }
            writer.push(EventKind::TurnFinished {
                turn,
                ok: captured.ok,
            });
            last_ok = captured.ok;
        }
        writer.push(EventKind::RunFinished {
            ok: finished && last_ok,
            exit_code: None,
            timed_out: false,
        });
        self.emit_run_recorded(job_id, writer.seal(finished, self.run_store().as_ref()));
    }

    /// Emit a `RunRecorded` announcement for a sealed run, if it persisted. A
    /// best-effort no-op when sealing was skipped (no served root / write failure).
    fn emit_run_recorded(&self, job_id: &str, sealed: Option<crate::run_store::SealedRun>) {
        if let Some(sealed) = sealed {
            self.emit(RuntimeEvent::run_recorded(
                job_id,
                sealed.run_id,
                sealed.root_hash,
                sealed.event_count,
            ));
        }
    }

    fn emit(&self, event: RuntimeEvent) {
        (self.emit)(event);
    }
}

/// Build a [`UsageUpdated`](nerve_core::provenance::EventKind::UsageUpdated) event
/// for one turn from a parsed [`DelegateUsage`](delegate_runtime::DelegateUsage) +
/// reported USD cost. Shared by the one-shot and live capture paths; cost is stored
/// as integer micro-USD (no floats in the hashed bytes — INV-R2).
/// Map a per-check status to the verdict vocabulary a receipt check carries.
fn check_status_to_verdict(
    status: nerve_core::verdict::CheckStatus,
) -> nerve_core::verdict::VerdictStatus {
    use nerve_core::verdict::{CheckStatus, VerdictStatus};
    match status {
        CheckStatus::Pass => VerdictStatus::Passed,
        CheckStatus::Fail => VerdictStatus::Failed,
        CheckStatus::Flaky => VerdictStatus::Inconclusive,
        CheckStatus::Error => VerdictStatus::Error,
    }
}

fn delegate_usage_event(
    turn: u64,
    usage: &delegate_runtime::DelegateUsage,
    cost_usd: Option<f64>,
) -> nerve_core::provenance::EventKind {
    nerve_core::provenance::EventKind::UsageUpdated {
        turn,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
        cost_micro_usd: crate::run_store::cost_to_micro_usd(cost_usd),
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

/// Serialize the host-shell capability surface reachable through the daemon.
fn host_capabilities_value() -> Result<Value, nerve_runtime::RuntimeError> {
    let (scheme, accent, accent_ink) = host_appearance();
    let caps = HostCapabilities::daemon_web(
        host_platform(),
        HostCapabilitySupport {
            clipboard_write_text: clipboard_write_text_supported(),
            os_notifications: notifications_supported(),
            native_file_dialogs: native_file_dialogs_supported(),
            external_url_open: external_url_open_supported(),
            system_color_scheme: scheme,
            system_accent_color: accent,
            system_accent_ink_color: accent_ink,
        },
    );
    serde_json::to_value(caps).map_err(|err| {
        nerve_runtime::RuntimeError::adapter(format!("serialize host capabilities: {err}"))
    })
}

fn run_clipboard_write_text(text: &str) -> Result<Value, nerve_runtime::RuntimeError> {
    validate_host_text("clipboard text", text)?;
    write_clipboard_text(text)?;
    Ok(json!({ "written": true, "bytes": text.len() }))
}

fn write_clipboard_text(text: &str) -> Result<(), nerve_runtime::RuntimeError> {
    let mut attempts = Vec::new();
    for (program, args) in clipboard_write_commands() {
        match write_clipboard_with_command(program, args, text) {
            Ok(()) => return Ok(()),
            Err(err) => attempts.push(format!("{program}: {err}")),
        }
    }
    Err(nerve_runtime::RuntimeError::adapter(format!(
        "host clipboard write unavailable ({})",
        attempts.join("; ")
    )))
}

fn write_clipboard_with_command(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(text.as_bytes())
            .map_err(|err| err.to_string()),
        None => Err("stdin unavailable".to_string()),
    };
    let status = child.wait().map_err(|err| err.to_string())?;
    write_result?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

fn clipboard_write_text_supported() -> bool {
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        return true;
    }
    clipboard_write_commands()
        .iter()
        .any(|(program, _)| program_available(program))
}

fn program_available(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

fn run_folder_pick(title: Option<&str>) -> Result<Value, nerve_runtime::RuntimeError> {
    let title = dialog_title(title, "Choose a project folder", "folder picker title")?;
    let path = pick_folder(&title)?;
    if path.is_empty() {
        return Err(nerve_runtime::RuntimeError::adapter(
            "folder picker returned an empty path",
        ));
    }
    Ok(json!({ "path": path }))
}

fn run_file_save_text(
    title: Option<&str>,
    default_name: Option<&str>,
    text: &str,
    token: &CancelToken,
) -> Result<Value, nerve_runtime::RuntimeError> {
    if token.is_cancelled() {
        return Err(nerve_runtime::RuntimeError::cancelled());
    }
    validate_host_text("file text", text)?;
    let title = dialog_title(title, "Save packet", "save panel title")?;
    let default_name = dialog_default_name(default_name)?;
    let path = pick_save_file(&title, &default_name)?;
    if token.is_cancelled() {
        return Err(nerve_runtime::RuntimeError::cancelled());
    }
    fs::write(&path, text.as_bytes()).map_err(|err| {
        nerve_runtime::RuntimeError::adapter(format!("write selected file `{path}`: {err}"))
    })?;
    Ok(json!({ "path": path, "bytes": text.len() }))
}

fn validate_host_text(label: &str, text: &str) -> Result<(), nerve_runtime::RuntimeError> {
    if text.len() > MAX_HOST_TEXT_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "{label} is too large: {} bytes exceeds {MAX_HOST_TEXT_BYTES}",
            text.len()
        )));
    }
    Ok(())
}

fn dialog_title(
    title: Option<&str>,
    fallback: &'static str,
    label: &'static str,
) -> Result<String, nerve_runtime::RuntimeError> {
    let title = title.unwrap_or(fallback).trim();
    if title.len() > MAX_DIALOG_TITLE_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "{label} is too large: {} bytes exceeds {MAX_DIALOG_TITLE_BYTES}",
            title.len()
        )));
    }
    Ok(if title.is_empty() { fallback } else { title }.to_string())
}

fn dialog_default_name(default_name: Option<&str>) -> Result<String, nerve_runtime::RuntimeError> {
    let raw = default_name.unwrap_or("nerve-packet.md").trim();
    if raw.len() > MAX_DIALOG_NAME_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "save default name is too large: {} bytes exceeds {MAX_DIALOG_NAME_BYTES}",
            raw.len()
        )));
    }
    let name = raw
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("nerve-packet.md");
    Ok(if name.is_empty() {
        "nerve-packet.md"
    } else {
        name
    }
    .to_string())
}

fn pick_folder(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_macos_folder_picker(title);
    }
    if cfg!(target_os = "windows") {
        return run_windows_folder_picker(title);
    }
    if cfg!(target_os = "linux") {
        return run_linux_folder_picker(title);
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native folder picker is unavailable on this platform",
    ))
}

fn pick_save_file(title: &str, default_name: &str) -> Result<String, nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_macos_save_panel(title, default_name);
    }
    if cfg!(target_os = "windows") {
        return run_windows_save_panel(title, default_name);
    }
    if cfg!(target_os = "linux") {
        return run_linux_save_panel(title, default_name);
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native save panel is unavailable on this platform",
    ))
}

fn run_url_open(url: &str) -> Result<Value, nerve_runtime::RuntimeError> {
    let url = validate_external_url(url)?;
    open_external_url(&url)?;
    Ok(json!({ "opened": true, "url": url }))
}

fn validate_external_url(url: &str) -> Result<String, nerve_runtime::RuntimeError> {
    let trimmed = url.trim();
    if trimmed.len() > MAX_URL_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "url is too large: {} bytes exceeds {MAX_URL_BYTES}",
            trimmed.len()
        )));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Ok(trimmed.to_string())
    } else {
        Err(nerve_runtime::RuntimeError::adapter(
            "host.url.open only accepts http(s) URLs",
        ))
    }
}

fn open_external_url(url: &str) -> Result<(), nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_status_command("open", &[url]);
    }
    if cfg!(target_os = "windows") {
        let Some(program) = windows_dialog_program() else {
            return Err(nerve_runtime::RuntimeError::adapter(
                "external URL opener is unavailable on this Windows host",
            ));
        };
        return run_status_command(
            program,
            &["-NoProfile", "-Command", "Start-Process $args[0]", url],
        );
    }
    if cfg!(target_os = "linux") {
        if program_available("xdg-open") {
            return run_status_command("xdg-open", &[url]);
        }
        if program_available("gio") {
            return run_status_command("gio", &["open", url]);
        }
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "external URL opener is unavailable on this platform",
    ))
}

fn run_notification_show(
    title: &str,
    body: Option<&str>,
) -> Result<Value, nerve_runtime::RuntimeError> {
    validate_notification_text("title", title, MAX_NOTIFICATION_TITLE_BYTES)?;
    let body = body.unwrap_or_default();
    validate_notification_text("body", body, MAX_NOTIFICATION_BODY_BYTES)?;
    show_notification(title.trim(), body.trim())?;
    Ok(json!({ "shown": true }))
}

fn validate_notification_text(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), nerve_runtime::RuntimeError> {
    if field == "title" && value.trim().is_empty() {
        return Err(nerve_runtime::RuntimeError::adapter(
            "notification title cannot be empty",
        ));
    }
    if value.len() > max_bytes {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "notification {field} is too large: {} bytes exceeds {max_bytes}",
            value.len()
        )));
    }
    Ok(())
}

fn show_notification(title: &str, body: &str) -> Result<(), nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_macos_notification(title, body);
    }
    if cfg!(target_os = "linux") && program_available("notify-send") {
        return run_status_command("notify-send", &[title, body]);
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "host notifications are unavailable on this platform",
    ))
}

fn run_macos_notification(title: &str, body: &str) -> Result<(), nerve_runtime::RuntimeError> {
    run_status_command(
        "osascript",
        &[
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            body,
        ],
    )
}

fn run_macos_folder_picker(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    run_output_command(
        "osascript",
        &[
            "-e",
            "on run argv",
            "-e",
            "set promptText to item 1 of argv",
            "-e",
            "set pickedFolder to choose folder with prompt promptText",
            "-e",
            "return POSIX path of pickedFolder",
            "-e",
            "end run",
            title,
        ],
        "osascript returned no folder path",
        "folder selection cancelled",
    )
}

fn run_macos_save_panel(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    run_output_command(
        "osascript",
        &[
            "-e",
            "on run argv",
            "-e",
            "set promptText to item 1 of argv",
            "-e",
            "set defaultName to item 2 of argv",
            "-e",
            "set pickedFile to choose file name with prompt promptText default name defaultName",
            "-e",
            "return POSIX path of pickedFile",
            "-e",
            "end run",
            title,
            default_name,
        ],
        "osascript returned no save path",
        "file save cancelled",
    )
}

fn run_linux_folder_picker(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    if program_available("zenity") {
        return run_output_command(
            "zenity",
            &["--file-selection", "--directory", "--title", title],
            "zenity returned no folder path",
            "folder selection cancelled",
        );
    }
    if program_available("kdialog") {
        return run_output_command(
            "kdialog",
            &["--title", title, "--getexistingdirectory"],
            "kdialog returned no folder path",
            "folder selection cancelled",
        );
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native folder picker is unavailable on this Linux host",
    ))
}

fn run_linux_save_panel(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    if program_available("zenity") {
        return run_output_command(
            "zenity",
            &[
                "--file-selection",
                "--save",
                "--confirm-overwrite",
                "--title",
                title,
                "--filename",
                default_name,
            ],
            "zenity returned no save path",
            "file save cancelled",
        );
    }
    if program_available("kdialog") {
        return run_output_command(
            "kdialog",
            &["--title", title, "--getsavefilename", default_name],
            "kdialog returned no save path",
            "file save cancelled",
        );
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native save panel is unavailable on this Linux host",
    ))
}

fn run_windows_folder_picker(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    let Some(program) = windows_dialog_program() else {
        return Err(nerve_runtime::RuntimeError::adapter(
            "native folder picker is unavailable on this Windows host",
        ));
    };
    run_output_command(
        program,
        &[
            "-NoProfile",
            "-STA",
            "-Command",
            WINDOWS_FOLDER_PICKER_SCRIPT,
            title,
        ],
        "PowerShell returned no folder path",
        "folder selection cancelled",
    )
}

fn run_windows_save_panel(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    let Some(program) = windows_dialog_program() else {
        return Err(nerve_runtime::RuntimeError::adapter(
            "native save panel is unavailable on this Windows host",
        ));
    };
    run_output_command(
        program,
        &[
            "-NoProfile",
            "-STA",
            "-Command",
            WINDOWS_SAVE_FILE_SCRIPT,
            title,
            default_name,
        ],
        "PowerShell returned no save path",
        "file save cancelled",
    )
}

fn run_output_command(
    program: &str,
    args: &[&str],
    empty_message: &'static str,
    cancel_message: &'static str,
) -> Result<String, nerve_runtime::RuntimeError> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| nerve_runtime::RuntimeError::adapter(format!("run {program}: {err}")))?;
    if !output.status.success() {
        return Err(nerve_runtime::RuntimeError::adapter(command_failure(
            program,
            output.status,
            &output.stderr,
            cancel_message,
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if text.is_empty() {
        return Err(nerve_runtime::RuntimeError::adapter(empty_message));
    }
    Ok(text)
}

fn run_status_command(program: &str, args: &[&str]) -> Result<(), nerve_runtime::RuntimeError> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| nerve_runtime::RuntimeError::adapter(format!("run {program}: {err}")))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(nerve_runtime::RuntimeError::adapter(format!(
                    "{program} exited with {status}"
                )))
            }
        })
}

fn command_failure(
    program: &str,
    status: std::process::ExitStatus,
    stderr: &[u8],
    cancel_message: &'static str,
) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if dialog_cancelled(status, &message) {
        return cancel_message.to_string();
    }
    if message.is_empty() {
        format!("{program} exited with {status}")
    } else {
        format!("{program} exited with {status}: {message}")
    }
}

fn dialog_cancelled(status: std::process::ExitStatus, message: &str) -> bool {
    let code = status.code();
    code == Some(2)
        || message.contains("User canceled")
        || message.contains("-128")
        || (message.is_empty() && code == Some(1))
}

fn notifications_supported() -> bool {
    if cfg!(target_os = "macos") {
        return program_available("osascript");
    }
    cfg!(target_os = "linux") && program_available("notify-send")
}

fn host_appearance() -> (Option<String>, Option<String>, Option<String>) {
    let scheme = system_color_scheme();
    let accent = system_accent_color();
    let accent_ink = accent.as_deref().and_then(accent_ink_color);
    (scheme, accent, accent_ink)
}

fn system_color_scheme() -> Option<String> {
    if cfg!(target_os = "macos") {
        return macos_color_scheme();
    }
    if cfg!(target_os = "windows") {
        return windows_color_scheme();
    }
    if cfg!(target_os = "linux") {
        return linux_color_scheme();
    }
    None
}

fn system_accent_color() -> Option<String> {
    if cfg!(target_os = "macos") {
        return macos_accent_color();
    }
    if cfg!(target_os = "windows") {
        return windows_accent_color();
    }
    if cfg!(target_os = "linux") {
        return linux_accent_color();
    }
    None
}

fn macos_color_scheme() -> Option<String> {
    if !program_available("defaults") {
        return None;
    }
    let output = Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && value.trim().eq_ignore_ascii_case("Dark") {
        Some("dark".into())
    } else {
        Some("light".into())
    }
}

fn macos_accent_color() -> Option<String> {
    let value = command_stdout("defaults", &["read", "-g", "AppleAccentColor"])?;
    match value.trim().parse::<i32>().ok()? {
        0 => Some("#ff3b30".into()),
        1 => Some("#ff9500".into()),
        2 => Some("#ffcc00".into()),
        3 => Some("#34c759".into()),
        4 => Some("#007aff".into()),
        5 => Some("#af52de".into()),
        6 => Some("#ff2d55".into()),
        _ => None,
    }
}

fn windows_color_scheme() -> Option<String> {
    let program = windows_dialog_program()?;
    let value = command_stdout(
        program,
        &[
            "-NoProfile",
            "-Command",
            "(Get-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize').AppsUseLightTheme",
        ],
    )?;
    match value.trim() {
        "0" => Some("dark".into()),
        "1" => Some("light".into()),
        _ => None,
    }
}

fn windows_accent_color() -> Option<String> {
    let program = windows_dialog_program()?;
    let value = command_stdout(
        program,
        &[
            "-NoProfile",
            "-Command",
            "(Get-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\DWM').AccentColor",
        ],
    )?;
    let raw = value.trim().parse::<i64>().ok()? as u32;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        raw & 0xff,
        (raw >> 8) & 0xff,
        (raw >> 16) & 0xff
    ))
}

fn linux_color_scheme() -> Option<String> {
    let value = command_stdout(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    )?;
    if value.contains("dark") {
        Some("dark".into())
    } else if value.contains("light") {
        Some("light".into())
    } else {
        None
    }
}

fn linux_accent_color() -> Option<String> {
    let value = command_stdout(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "accent-color"],
    )?;
    match value.trim_matches(['\'', '"', ' ']) {
        "blue" => Some("#3584e4".into()),
        "teal" => Some("#2190a4".into()),
        "green" => Some("#3a944a".into()),
        "yellow" => Some("#c88800".into()),
        "orange" => Some("#ed5b00".into()),
        "red" => Some("#e62d42".into()),
        "pink" => Some("#d56199".into()),
        "purple" => Some("#9141ac".into()),
        "slate" => Some("#6f8396".into()),
        _ => None,
    }
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn accent_ink_color(color: &str) -> Option<String> {
    let (r, g, b) = parse_hex_color(color)?;
    let luminance = u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114;
    Some(
        if luminance > 150_000 {
            "#111111"
        } else {
            "#ffffff"
        }
        .into(),
    )
}

fn parse_hex_color(color: &str) -> Option<(u8, u8, u8)> {
    let hex = color.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}

fn native_file_dialogs_supported() -> bool {
    if cfg!(target_os = "macos") {
        return program_available("osascript");
    }
    if cfg!(target_os = "windows") {
        return windows_dialog_program().is_some();
    }
    cfg!(target_os = "linux") && (program_available("zenity") || program_available("kdialog"))
}

fn external_url_open_supported() -> bool {
    if cfg!(target_os = "macos") {
        return program_available("open");
    }
    if cfg!(target_os = "windows") {
        return windows_dialog_program().is_some();
    }
    cfg!(target_os = "linux") && (program_available("xdg-open") || program_available("gio"))
}

fn windows_dialog_program() -> Option<&'static str> {
    ["powershell.exe", "powershell", "pwsh.exe", "pwsh"]
        .into_iter()
        .find(|program| program_available(program))
}

fn host_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

/// Platform clipboard writer commands, ordered by preference.
fn clipboard_write_commands() -> &'static [(&'static str, &'static [&'static str])] {
    const NO_ARGS: &[&str] = &[];
    const XCLIP_ARGS: &[&str] = &["-selection", "clipboard"];
    const XSEL_ARGS: &[&str] = &["--clipboard", "--input"];
    const MACOS: &[(&str, &[&str])] = &[("pbcopy", NO_ARGS)];
    const WINDOWS: &[(&str, &[&str])] = &[("clip", NO_ARGS)];
    const LINUX: &[(&str, &[&str])] = &[
        ("wl-copy", NO_ARGS),
        ("xclip", XCLIP_ARGS),
        ("xsel", XSEL_ARGS),
    ];
    if cfg!(target_os = "macos") {
        MACOS
    } else if cfg!(target_os = "windows") {
        WINDOWS
    } else {
        LINUX
    }
}

/// The OS file-manager opener for the current platform (used by `workspace.reveal`).
fn workspace_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    }
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
    fn host_url_validation_accepts_only_external_http_urls() {
        assert_eq!(
            validate_external_url(" https://example.com/auth ").expect("https URL accepted"),
            "https://example.com/auth"
        );
        assert_eq!(
            validate_external_url("http://localhost:1455/auth/callback")
                .expect("http loopback URL accepted"),
            "http://localhost:1455/auth/callback"
        );

        for url in [
            "file:///tmp/secret",
            "nerve://auth/callback",
            "javascript:alert(1)",
            "mailto:security@example.com",
            "",
        ] {
            assert!(
                validate_external_url(url).is_err(),
                "non-http(s) URL `{url}` must not reach the OS opener"
            );
        }

        let oversized = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(
            validate_external_url(&oversized).is_err(),
            "oversized URL must be rejected before invoking the host opener"
        );
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
