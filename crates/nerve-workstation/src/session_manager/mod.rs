//! Workstation-owned Session Manager for protocol `session.*` commands.
//!
//! `nerve-runtime` defines session commands/events as transport-neutral data;
//! this module is the daemon composition root that executes them with
//! `nerve-agent`, policy, provider registry, persistence, and runtime event
//! emission.

mod approval;

use crate::capabilities::{Capabilities, ResolvedAgent};
use crate::checkpoint::Checkpoint;
use crate::policy::{Policy, ToolGate};
use crate::session::{SessionRecord, SessionStore};
use crate::subagent::{DEFAULT_MAX_DEPTH, SubAgentSpawner};
use crate::{agent, providers::ProviderRegistry, tools};
use nerve_agent::{AgentEvent, Message};
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{RuntimeCommand, RuntimeError, RuntimeEvent};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use approval::{ApprovalHub, ProtocolApprover};

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
            RuntimeCommand::SessionSetModel {
                session_id,
                provider,
                model,
            } => self.set_model(&session_id, provider, model),
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
        crate::sync::lock_recover(&self.sessions).insert(id.clone(), session);
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
        let mut sessions = crate::sync::lock_recover(&self.sessions);
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
        let mut sessions = crate::sync::lock_recover(&self.sessions);
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
        let sessions = crate::sync::lock_recover(&self.sessions);
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
        let sessions = crate::sync::lock_recover(&self.sessions);
        let session = sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::adapter(format!("unknown session: {session_id}")))?;
        Ok(json!({ "session": session.snapshot() }))
    }

    fn list(&self) -> Vec<Value> {
        let mut sessions: Vec<_> = crate::sync::lock_recover(&self.sessions)
            .values()
            .map(LiveSession::snapshot)
            .collect();
        sessions.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
        sessions
    }

    fn close(&self, session_id: &str) -> Result<Value, RuntimeError> {
        let removed = {
            let mut sessions = crate::sync::lock_recover(&self.sessions);
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

    /// Swap a live session's model/provider in place; affects the next turn only.
    fn set_model(
        &self,
        session_id: &str,
        provider: Option<String>,
        model: String,
    ) -> Result<Value, RuntimeError> {
        let mut sessions = crate::sync::lock_recover(&self.sessions);
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| RuntimeError::adapter(format!("unknown session: {session_id}")))?;
        session.retarget(provider, model);
        Ok(json!({ "session": session.snapshot() }))
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
    /// Point this session at a new model/provider for its next turn.
    fn retarget(&mut self, provider: Option<String>, model: String) {
        if let Some(provider) = provider {
            self.config.provider = provider;
        }
        self.config.model = model;
    }

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
        verify_completion: false,
        // Daemon session turns refuse exec by trust context, not just by flag.
        allow_exec: false,
        exec_launcher: crate::sandbox::refuse_launcher(),
    }
}

fn map_session_agent_event(session_id: &str, event: AgentEvent) -> Option<RuntimeEvent> {
    crate::agent_event::agent_event_kind(event)
        .map(|kind| RuntimeEvent::session_agent(session_id.to_string(), kind))
}

#[cfg(test)]
mod tests {
    use super::approval::{ApprovalHub, ProtocolApprover};
    use super::*;
    use crate::policy::Approver;
    use nerve_runtime::SessionApprovalDecision;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn session_runs_refuse_exec_by_trust_context() {
        use crate::sandbox::{CommandSpec, SandboxPolicy};
        // A daemon session turn must never execute, independent of any capability
        // flag: the run config carries allow_exec=false AND a refusing launcher.
        // Pins the safety invariant "daemon REFUSES exec" at the session path.
        let config = SessionConfig {
            workspace: None,
            provider: "claude".into(),
            model: "m".into(),
            system_prompt: None,
            agent: None,
            max_turns: None,
            temperature: None,
            reasoning_effort: None,
            tool_filter: None,
        };
        let run = session_run_config(&config, ResolvedAgent::default(), "do work");
        assert!(!run.allow_exec, "session exec capability must be off");
        let spec = CommandSpec {
            command: "ls".into(),
            args: Vec::new(),
        };
        assert!(
            run.exec_launcher
                .launch(&spec, &SandboxPolicy::for_root(None), &CancelToken::never())
                .is_err(),
            "session launcher must refuse execution"
        );
    }

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

    #[test]
    fn retarget_switches_model_and_keeps_history() {
        let mut session = LiveSession {
            id: "s1".into(),
            config: SessionConfig {
                workspace: None,
                provider: "claude".into(),
                model: "m1".into(),
                system_prompt: None,
                agent: None,
                max_turns: None,
                temperature: None,
                reasoning_effort: None,
                tool_filter: None,
            },
            history: vec![Message::user("hi")],
            record: SessionRecord::begin("claude", "m1", ""),
            checkpoint: new_checkpoint(None),
            status: SessionStatus::Idle,
            current_cancel: None,
        };
        session.retarget(Some("xai".into()), "grok-4-fast".into());
        assert_eq!(session.config.provider, "xai");
        assert_eq!(session.config.model, "grok-4-fast");
        assert_eq!(session.history.len(), 1);
        let snapshot = session.snapshot();
        assert_eq!(snapshot["provider"], "xai");
        assert_eq!(snapshot["model"], "grok-4-fast");
    }
}
