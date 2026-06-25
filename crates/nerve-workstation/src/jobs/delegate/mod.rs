//! Delegation (`delegate.*`) command executors for the [`JobManager`].
//!
//! `delegate.start` (DA-2/DA-5a) resolves the agent and either runs a one-shot CLI
//! (codex/gemini) or starts a live, steerable `claude` session that parks for later
//! steering; `delegate.steer` / `delegate.close` (DA-5a) route into the live-session
//! registry by session id. The L0 run-capture seal/emit tail lives in the sibling
//! [`seal`] module. All executors are `impl JobManager` methods so they reach the
//! manager's private launcher / live-session registry / served-root resolver directly.

mod seal;

use super::JobManager;
use crate::delegate_proxy::{DelegateDecisions, DelegateProxy};
use crate::delegate_runtime::{self, DelegateAgent, DelegateError, DelegateParser};
use crate::delegate_session::DelegateSession;
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{DelegateAutonomy, DelegateRole, RuntimeCommand, RuntimeEvent};
use serde_json::{Value, json};
use std::sync::Arc;

impl JobManager {
    /// Execute a `delegate.*` command. `delegate.start` (DA-2/DA-5a) resolves the
    /// agent and either runs a one-shot CLI (codex/gemini) or starts a live,
    /// steerable `claude` session that parks for later steering; `delegate.steer`
    /// and `delegate.close` (DA-5a) route into the live-session registry by
    /// session id. A refusing `delegate_launcher` (the default trust context)
    /// surfaces a clear "delegation is disabled" error instead of spawning.
    pub(super) fn run_delegate_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::DelegateStart {
                agent,
                task,
                workspace,
                cwd,
                autonomy,
                role,
                model,
                mcp_enable,
            } => self.run_delegate_start(
                job_id, &agent, &task, workspace, cwd, autonomy, role, model, mcp_enable, token,
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
        workspace: Option<String>,
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
        // Confine to the ACTIVE workspace's root (the GUI passes which one); only when
        // no workspace is given does this fall back to the sole/default workspace.
        let root = self.served_root(workspace.as_deref())?;
        let run_cwd = delegate_runtime::resolve_delegate_cwd(&root, cwd.as_deref())
            .map_err(delegate_error)?;
        // L3↔L1: commit the authorization posture to the evidence ledger once the request
        // is validated (agent + cwd) and about to launch — both the live and one-shot
        // paths inherit it, and (mirroring the in-chat path) a rejected request is not
        // recorded as an authorization on either face.
        self.record_delegate_authorization(job_id, agent, role, autonomy);
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
        let started_at_ms = super::now_ms();
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
        // Wave 3: lift turn 1's structured tool calls into LIVE per-tool `DelegateAgent`
        // rows so a client renders them alongside the (retained) `DelegateProgress`
        // text tail — claude/codex on the live path index at seal, this surfaces the
        // same lift on the wire as the turn finishes.
        self.emit_delegate_tool_rows(job_id, resolved, &turn.raw_lines, 0);
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
        // Wave 3: lift this steer turn's structured tool calls into LIVE per-tool
        // `DelegateAgent` rows (keyed by the session/job id), alongside the retained
        // `DelegateProgress` text tail — symmetric with the start turn.
        if let Ok(resolved) = DelegateAgent::from_name(agent) {
            self.emit_delegate_tool_rows(session_id, resolved, &turn.raw_lines, 0);
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
    pub(super) fn run_store(&self) -> Option<crate::run_store::RunStore> {
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
    /// Resolve the served root for a specific `workspace` (the ACTIVE one for a delegated
    /// run), falling back to the sole/default workspace when `None`. Errors when the
    /// resolved workspace has no root, or when the choice is ambiguous (more than one
    /// workspace registered and none specified) — the case the GUI hits after adding a
    /// project, which is why delegation must pass the active workspace.
    fn served_root(
        &self,
        workspace: Option<&str>,
    ) -> Result<std::path::PathBuf, nerve_runtime::RuntimeError> {
        self.runtime
            .resolver()
            .resolve_workspace(workspace)
            .ok()
            .and_then(|workspace| workspace.roots().first().map(|root| root.path.clone()))
            .ok_or_else(|| {
                nerve_runtime::RuntimeError::adapter(
                    "delegation requires a served workspace root (start the daemon with --root)",
                )
            })
    }

    /// The served root for the sole/default workspace — the scope the trust-substrate
    /// stores (run/ledger/verdict/receipt/outcome) resolve their `.nerve/` dir against.
    pub(super) fn delegate_root(&self) -> Result<std::path::PathBuf, nerve_runtime::RuntimeError> {
        self.served_root(None)
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
                // L0 granularity: index the tools/commands/edits this line carries,
                // in tape order right after its raw Output (gemini -> empty). Each
                // lifted kind is ALSO emitted live as a structured `DelegateAgent`
                // per-tool row (Wave 3) so a client renders it in real time, in
                // order, alongside the retained `DelegateProgress` text tail.
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                    seal::lift_tool_events_into_tape(resolved, &value, 0, &emit, &job, &mut writer);
                }
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
