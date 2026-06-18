//! Workstation-owned Session Manager for protocol `session.*` commands.
//!
//! `nerve-runtime` defines session commands/events as transport-neutral data;
//! this module is the daemon composition root that executes them with
//! `nerve-agent`, policy, provider registry, persistence, and runtime event
//! emission.

use crate::capabilities::{Capabilities, ResolvedAgent};
use crate::checkpoint::Checkpoint;
use crate::policy::{Approver, Policy, ToolGate};
use crate::session::{SessionRecord, SessionStore};
use crate::subagent::{DEFAULT_MAX_DEPTH, SubAgentSpawner};
use crate::{agent, providers::ProviderRegistry, tools};
use nerve_agent::{AgentEvent, Message};
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{
    AgentEventKind, RuntimeCommand, RuntimeError, RuntimeEvent, SessionApprovalDecision,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

const APPROVAL_POLL: Duration = Duration::from_millis(100);

type EventEmitter = dyn Fn(RuntimeEvent) + Send + Sync + 'static;
type SessionCheckpoint = Arc<Mutex<Checkpoint>>;
type ResumeRecord = (String, Vec<Message>, SessionRecord, SessionCheckpoint);

pub(crate) struct SessionManager {
    runtime: Arc<tools::NerveRuntime>,
    registry: ProviderRegistry,
    policy: Policy,
    store: Option<SessionStore>,
    sessions: Mutex<HashMap<String, LiveSession>>,
    approvals: Arc<ApprovalHub>,
    emit: Arc<EventEmitter>,
}

struct LiveSession {
    id: String,
    config: SessionConfig,
    history: Vec<Message>,
    record: SessionRecord,
    checkpoint: SessionCheckpoint,
    status: SessionStatus,
    current_cancel: Option<CancelToken>,
}

#[derive(Clone)]
struct SessionConfig {
    workspace: Option<String>,
    provider: String,
    model: String,
    system_prompt: Option<String>,
    agent: Option<String>,
    max_turns: Option<u32>,
    temperature: Option<f32>,
    reasoning_effort: Option<String>,
    tool_filter: Option<Vec<String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionStatus {
    Idle,
    Running,
    Closed,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Closed => "closed",
        }
    }
}

impl SessionManager {
    pub(crate) fn new(
        runtime: Arc<tools::NerveRuntime>,
        registry: ProviderRegistry,
        policy: Policy,
        store: Option<SessionStore>,
        emit: Arc<EventEmitter>,
    ) -> Self {
        Self {
            runtime,
            registry,
            policy,
            store,
            sessions: Mutex::new(HashMap::new()),
            approvals: Arc::new(ApprovalHub::new(Arc::clone(&emit))),
            emit,
        }
    }

    pub(crate) fn handle_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        match command {
            RuntimeCommand::SessionStart {
                workspace,
                provider,
                model,
                system_prompt,
                agent,
                resume,
                max_turns,
                temperature,
                reasoning_effort,
                tool_filter,
            } => self.start(
                SessionConfig {
                    workspace,
                    provider,
                    model,
                    system_prompt,
                    agent,
                    max_turns,
                    temperature,
                    reasoning_effort,
                    tool_filter,
                },
                resume,
            ),
            RuntimeCommand::SessionMessage { session_id, text } => {
                self.message(&session_id, &text, token)
            }
            RuntimeCommand::SessionInterrupt { session_id } => self.interrupt(&session_id),
            RuntimeCommand::SessionRespond {
                session_id,
                request_id,
                decision,
            } => Ok(json!({
                "responded": self.approvals.respond(&session_id, &request_id, decision)
            })),
            RuntimeCommand::SessionGet { session_id } => self.get(&session_id),
            RuntimeCommand::SessionList => Ok(json!({ "sessions": self.list() })),
            RuntimeCommand::SessionClose { session_id } => self.close(&session_id),
            _ => Err(RuntimeError::adapter("expected session.* command")),
        }
    }

    fn start(&self, config: SessionConfig, resume: Option<String>) -> Result<Value, RuntimeError> {
        let (id, history, record, checkpoint) = match resume {
            Some(id) => self.resume_record(&id)?,
            None => {
                let record = SessionRecord::begin(&config.provider, &config.model, "");
                let checkpoint = new_checkpoint(None);
                (record.id.clone(), Vec::new(), record, checkpoint)
            }
        };
        let session = LiveSession {
            id: id.clone(),
            config,
            history,
            record,
            checkpoint,
            status: SessionStatus::Idle,
            current_cancel: None,
        };
        self.sessions
            .lock()
            .expect("session lock")
            .insert(id.clone(), session);
        self.emit(RuntimeEvent::session_started(id.clone()));
        Ok(json!({ "session_id": id }))
    }

    fn resume_record(&self, id: &str) -> Result<ResumeRecord, RuntimeError> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| RuntimeError::adapter("session resume unavailable: no session store"))?;
        let record = store.load(id).map_err(|err| {
            RuntimeError::adapter(format!("failed to resume session {id}: {err}"))
        })?;
        let history = record.reconstructed_history();
        let checkpoint = new_checkpoint(record.restore_with_staleness());
        Ok((record.id.clone(), history, record, checkpoint))
    }

    fn message(
        &self,
        session_id: &str,
        text: &str,
        token: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        let (config, history, checkpoint) = self.begin_turn(session_id, text, token)?;
        self.emit(RuntimeEvent::turn_started(session_id.to_string()));
        let result = self.run_turn(session_id, &config, history, checkpoint, text, token);
        self.finish_turn(session_id, result, token)
    }

    fn begin_turn(
        &self,
        session_id: &str,
        text: &str,
        token: &CancelToken,
    ) -> Result<(SessionConfig, Vec<Message>, SessionCheckpoint), RuntimeError> {
        let mut sessions = self.sessions.lock().expect("session lock");
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| RuntimeError::adapter(format!("unknown session: {session_id}")))?;
        if session.status == SessionStatus::Running {
            return Err(RuntimeError::adapter(format!(
                "session {session_id} is already running"
            )));
        }
        if session.record.task.is_empty() {
            session.record.task = text.to_string();
        }
        session.status = SessionStatus::Running;
        session.current_cancel = Some(token.clone());
        Ok((
            session.config.clone(),
            session.history.clone(),
            Arc::clone(&session.checkpoint),
        ))
    }

    fn run_turn(
        &self,
        session_id: &str,
        config: &SessionConfig,
        history: Vec<Message>,
        checkpoint: SessionCheckpoint,
        text: &str,
        token: &CancelToken,
    ) -> Result<TurnResult, RuntimeError> {
        let root = self.root_for(config.workspace.as_deref());
        let resolved = self.resolve_agent(config, root.as_deref())?;
        let run_config = session_run_config(config, resolved, text);
        let gate = ToolGate::with_approver(
            self.policy.clone(),
            Arc::new(ProtocolApprover::new(
                session_id.to_string(),
                Arc::clone(&self.approvals),
                token.clone(),
            )),
        );
        let spawner = SubAgentSpawner::new(
            Arc::clone(&self.runtime),
            self.registry.clone(),
            gate,
            DEFAULT_MAX_DEPTH,
            checkpoint,
        );
        let emit = Arc::clone(&self.emit);
        let session = session_id.to_string();
        let mut sink = |event: AgentEvent| {
            if let Some(runtime_event) = map_session_agent_event(&session, event) {
                emit(runtime_event);
            }
        };
        match spawner.run_at_depth(0, run_config, history, token, &mut sink) {
            Ok(output) => Ok(TurnResult {
                history: output.history,
                events: output.events,
                outcome: Some(output.outcome),
            }),
            Err(_) if token.is_cancelled() => Err(RuntimeError::cancelled()),
            Err(err) => Err(RuntimeError::adapter(err.to_string())),
        }
    }

    fn root_for(&self, workspace: Option<&str>) -> Option<std::path::PathBuf> {
        self.runtime
            .resolver()
            .resolve_workspace(workspace)
            .ok()
            .and_then(|workspace| workspace.roots().first().map(|root| root.path.clone()))
    }

    fn resolve_agent(
        &self,
        config: &SessionConfig,
        root: Option<&std::path::Path>,
    ) -> Result<ResolvedAgent, RuntimeError> {
        match config.agent.as_deref() {
            Some(name) => Capabilities::discover(root)
                .resolve_agent(name)
                .map_err(|err| RuntimeError::adapter(err.to_string())),
            None => Ok(ResolvedAgent::default()),
        }
    }

    fn finish_turn(
        &self,
        session_id: &str,
        result: Result<TurnResult, RuntimeError>,
        token: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        let mut sessions = self.sessions.lock().expect("session lock");
        let Some(session) = sessions.get_mut(session_id) else {
            return Err(RuntimeError::adapter(format!(
                "unknown session: {session_id}"
            )));
        };
        session.status = SessionStatus::Idle;
        session.current_cancel = None;
        if let Ok(turn) = &result {
            for event in &turn.events {
                session.record.push_event(event);
            }
            session.history = turn.history.clone();
            session.record.set_history(turn.history.clone());
            if let Some(outcome) = &turn.outcome {
                session.record.finish(Some(outcome));
            }
            session
                .record
                .set_checkpoint(Some(checkpoint_note(&session.checkpoint)));
            self.persist(&session.record);
        }
        drop(sessions);
        self.emit(RuntimeEvent::session_idle(session_id.to_string()));
        match result {
            Ok(turn) => Ok(json!({
                "session_id": session_id,
                "reason": turn.outcome.as_ref().map(|outcome| outcome.reason.as_str()),
                "turns": turn.outcome.as_ref().map(|outcome| outcome.turns),
                "final_text": turn.outcome.as_ref().map(|outcome| outcome.final_text.as_str()),
            })),
            Err(_) if token.is_cancelled() => Err(RuntimeError::cancelled()),
            Err(err) => Err(err),
        }
    }

    fn persist(&self, record: &SessionRecord) {
        if let Some(store) = &self.store
            && let Err(err) = store.write(record)
        {
            eprintln!("⚠  failed to persist session {}: {err}", record.id);
        }
    }

    fn interrupt(&self, session_id: &str) -> Result<Value, RuntimeError> {
        let sessions = self.sessions.lock().expect("session lock");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::adapter(format!("unknown session: {session_id}")))?;
        let interrupted = session.current_cancel.as_ref().is_some_and(|cancel| {
            cancel.cancel();
            true
        });
        Ok(json!({ "interrupted": interrupted }))
    }

    fn get(&self, session_id: &str) -> Result<Value, RuntimeError> {
        let sessions = self.sessions.lock().expect("session lock");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::adapter(format!("unknown session: {session_id}")))?;
        Ok(json!({ "session": session.snapshot() }))
    }

    fn list(&self) -> Vec<Value> {
        let mut sessions: Vec<_> = self
            .sessions
            .lock()
            .expect("session lock")
            .values()
            .map(LiveSession::snapshot)
            .collect();
        sessions.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
        sessions
    }

    fn close(&self, session_id: &str) -> Result<Value, RuntimeError> {
        let removed = {
            let mut sessions = self.sessions.lock().expect("session lock");
            let Some(mut session) = sessions.remove(session_id) else {
                return Err(RuntimeError::adapter(format!(
                    "unknown session: {session_id}"
                )));
            };
            if let Some(cancel) = session.current_cancel.take() {
                cancel.cancel();
            }
            session.status = SessionStatus::Closed;
            self.persist(&session.record);
            true
        };
        self.emit(RuntimeEvent::session_closed(session_id.to_string()));
        Ok(json!({ "closed": removed }))
    }

    fn emit(&self, event: RuntimeEvent) {
        (self.emit)(event);
    }
}

struct TurnResult {
    history: Vec<Message>,
    events: Vec<AgentEvent>,
    outcome: Option<nerve_agent::RunOutcome>,
}

fn new_checkpoint(note: Option<String>) -> SessionCheckpoint {
    let checkpoint = Arc::new(Mutex::new(Checkpoint::new()));
    if let Some(note) = note {
        lock_checkpoint(&checkpoint).replace(note);
    }
    checkpoint
}

fn checkpoint_note(checkpoint: &SessionCheckpoint) -> String {
    lock_checkpoint(checkpoint).note.clone()
}

fn lock_checkpoint(checkpoint: &SessionCheckpoint) -> std::sync::MutexGuard<'_, Checkpoint> {
    match checkpoint.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl LiveSession {
    fn snapshot(&self) -> Value {
        json!({
            "session_id": self.id,
            "status": self.status.as_str(),
            "provider": self.config.provider,
            "model": self.config.model,
            "agent": self.config.agent,
            "history_len": self.history.len(),
            "pending_approval": false,
        })
    }
}

struct ApprovalHub {
    pending: Mutex<HashMap<ApprovalKey, mpsc::Sender<SessionApprovalDecision>>>,
    next_id: AtomicU64,
    emit: Arc<EventEmitter>,
}

#[derive(Hash, PartialEq, Eq)]
struct ApprovalKey {
    session_id: String,
    request_id: String,
}

impl ApprovalHub {
    fn new(emit: Arc<EventEmitter>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            emit,
        }
    }

    fn request(
        &self,
        session_id: &str,
        tool: &str,
        arguments: &Value,
        cancel: &CancelToken,
    ) -> bool {
        let request_id = format!("approval-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::channel();
        let key = ApprovalKey {
            session_id: session_id.to_string(),
            request_id: request_id.clone(),
        };
        self.pending
            .lock()
            .expect("approval lock")
            .insert(key, sender);
        (self.emit)(RuntimeEvent::approval_requested(
            session_id.to_string(),
            request_id.clone(),
            tool.to_string(),
            arguments.clone(),
        ));
        let decision = loop {
            if cancel.is_cancelled() {
                break SessionApprovalDecision::Deny;
            }
            match receiver.recv_timeout(APPROVAL_POLL) {
                Ok(decision) => break decision,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break SessionApprovalDecision::Deny,
            }
        };
        self.pending
            .lock()
            .expect("approval lock")
            .remove(&ApprovalKey {
                session_id: session_id.to_string(),
                request_id,
            });
        decision == SessionApprovalDecision::Allow
    }

    fn respond(
        &self,
        session_id: &str,
        request_id: &str,
        decision: SessionApprovalDecision,
    ) -> bool {
        let key = ApprovalKey {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
        };
        self.pending
            .lock()
            .expect("approval lock")
            .remove(&key)
            .is_some_and(|sender| sender.send(decision).is_ok())
    }
}

struct ProtocolApprover {
    session_id: String,
    hub: Arc<ApprovalHub>,
    cancel: CancelToken,
}

impl ProtocolApprover {
    fn new(session_id: String, hub: Arc<ApprovalHub>, cancel: CancelToken) -> Self {
        Self {
            session_id,
            hub,
            cancel,
        }
    }
}

impl Approver for ProtocolApprover {
    fn approve(&self, tool: &str, args: &Value) -> bool {
        self.hub.request(&self.session_id, tool, args, &self.cancel)
    }
}

fn session_run_config(
    config: &SessionConfig,
    resolved: ResolvedAgent,
    task: &str,
) -> agent::AgentRunConfig {
    agent::AgentRunConfig {
        workspace: config.workspace.clone(),
        provider: config.provider.clone(),
        model: config.model.clone(),
        task: task.to_string(),
        system_prompt: config.system_prompt.clone().or(resolved.system_prompt),
        max_turns: config.max_turns.or(resolved.max_turns),
        temperature: config.temperature.or(resolved.temperature),
        reasoning_effort: config
            .reasoning_effort
            .clone()
            .or(resolved.reasoning_effort),
        tool_filter: config.tool_filter.clone().or(resolved.tool_filter),
        api_key: None,
        distill_memory: false,
    }
}

fn map_session_agent_event(session_id: &str, event: AgentEvent) -> Option<RuntimeEvent> {
    let kind = match event {
        AgentEvent::TurnStarted(turn) => AgentEventKind::TurnStarted {
            turn: u64::from(turn),
        },
        AgentEvent::AssistantText(text) => AgentEventKind::Message { text },
        AgentEvent::Reasoning(text) => AgentEventKind::Reasoning { text },
        AgentEvent::ToolStarted { name, args } => AgentEventKind::ToolStarted {
            tool: name,
            arguments: args,
        },
        AgentEvent::ToolFinished { name, ok, output } => AgentEventKind::ToolFinished {
            tool: name,
            ok,
            output,
        },
        AgentEvent::Interrupted(reason) => AgentEventKind::Interrupted { reason },
        AgentEvent::Done { .. } => return None,
    };
    Some(RuntimeEvent::session_agent(session_id.to_string(), kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::thread;

    #[test]
    fn protocol_approver_allows_via_channel() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let hub = Arc::new(ApprovalHub::new(Arc::new(move |event| {
            captured.lock().expect("events lock").push(event);
        })));
        let approver = ProtocolApprover::new("s1".into(), Arc::clone(&hub), CancelToken::never());
        let handle = thread::spawn(move || approver.approve("edit", &json!({"path":"x"})));

        let request_id = wait_for_request(&events);
        assert!(hub.respond("s1", &request_id, SessionApprovalDecision::Allow));
        assert!(handle.join().expect("approval thread"));
    }

    #[test]
    fn protocol_approver_denies_via_channel() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let hub = Arc::new(ApprovalHub::new(Arc::new(move |event| {
            captured.lock().expect("events lock").push(event);
        })));
        let approver = ProtocolApprover::new("s1".into(), Arc::clone(&hub), CancelToken::never());
        let handle = thread::spawn(move || approver.approve("edit", &json!({"path":"x"})));

        let request_id = wait_for_request(&events);
        assert!(hub.respond("s1", &request_id, SessionApprovalDecision::Deny));
        assert!(!handle.join().expect("approval thread"));
    }

    fn wait_for_request(events: &Mutex<Vec<RuntimeEvent>>) -> String {
        for _ in 0..50 {
            if let Some(RuntimeEvent::ApprovalRequested { request_id, .. }) =
                events.lock().expect("events lock").first().cloned()
            {
                return request_id;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("approval request not emitted")
    }
}
