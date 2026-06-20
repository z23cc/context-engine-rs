//! [`CliWorker`] — the [`AgentWorker`] over the external-CLI delegate substrate.
//!
//! This wraps the SHIPPED persistent delegate drivers without changing them:
//! claude/codex run as a live, steerable [`LiveDriver`](crate::delegate_live)
//! (the same [`DelegateSession`]/[`CodexSession`] the daemon's
//! `run_delegate_live` registers), and `gemini` runs one-shot through the
//! [`delegate_runtime`](crate::delegate_runtime) streaming recipe + parser. The
//! approval round-trip reuses [`delegate_proxy`](crate::delegate_proxy) verbatim,
//! routed to `ctx.approver` (the one [`ApprovalHub`]); a `can_use_tool` ask is
//! additionally re-projected as a [`WorkerEvent::Approval`] so the engine can
//! render it kind-agnostically.
//!
//! Credentials are NOT passed in: the CLI authenticates with its own on-disk login
//! and the [`delegate_runtime`] env scrub strips `*_KEY`/`*_TOKEN` — the natural
//! quota-isolation boundary the design relies on.
//!
//! [`ApprovalHub`]: crate::session_manager::ApprovalHub
//! [`DelegateSession`]: crate::delegate_session::DelegateSession
//! [`CodexSession`]: crate::delegate_session_codex::CodexSession
#![allow(
    dead_code,
    reason = "C0 worker port awaits its C1 engine caller (mirrors subagent::bounded_fan_out)"
)]

use super::{
    AgentWorker, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerKind, WorkerSession,
    WorkerTask, synthesize_turn_steps,
};
use crate::delegate_live::LiveDriver;
use crate::delegate_proxy::{DelegateApprover, DelegateDecisions, DelegateProxy};
use crate::delegate_runtime::{
    self, DelegateAgent, DelegateOutcome, DelegateParser, DelegateUsage,
};
use crate::sandbox::SandboxLauncher;
use nerve_core::CancelToken;
use nerve_runtime::{DelegateAutonomy, RiskTier, SessionApprovalDecision};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A worker backed by an external agentic CLI. Holds the shared delegate launcher
/// (the trust-bound [`SandboxLauncher`]) and the codex MCP-disable flags; `start`
/// spawns the right driver for its [`DelegateAgent`].
pub(crate) struct CliWorker {
    agent: DelegateAgent,
    launcher: Arc<dyn SandboxLauncher>,
    /// Pre-computed codex `-c mcp_servers.<n>.enabled=false` pairs (DA-6); empty
    /// for claude/gemini, which ignore them.
    mcp_disable_flags: Vec<String>,
}

impl CliWorker {
    /// Build a CLI worker for `agent`, sharing the launcher + codex MCP flags.
    pub(crate) fn new(
        agent: DelegateAgent,
        launcher: Arc<dyn SandboxLauncher>,
        mcp_disable_flags: Vec<String>,
    ) -> Self {
        Self {
            agent,
            launcher,
            mcp_disable_flags,
        }
    }

    /// Resolve the confined cwd for this run against the context root, reusing the
    /// shipped `resolve_delegate_cwd` (the `..`-escape rejection). Falls back to a
    /// bare cwd-less spawn error when no root is pinned.
    fn run_cwd(&self, ctx: &WorkerContext) -> Result<PathBuf, WorkerError> {
        let root = ctx.root.as_deref().ok_or_else(|| {
            WorkerError::Start("a CLI worker needs a workspace root to confine its cwd".into())
        })?;
        delegate_runtime::resolve_delegate_cwd(root, None)
            .map_err(|err| WorkerError::Start(err.to_string()))
    }
}

impl AgentWorker for CliWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Cli(self.agent.catalog_name())
    }

    fn capability(&self) -> RiskTier {
        // A delegated CLI can run arbitrary commands inside its sandbox; worst-case
        // is the exec tier regardless of the autonomy the task requests.
        RiskTier::Exec
    }

    fn start(
        &self,
        task: &WorkerTask,
        ctx: &WorkerContext,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let cwd = self.run_cwd(ctx)?;
        match self.agent {
            DelegateAgent::Gemini => self.start_gemini(task, &cwd, cancel, on_event),
            _ => self.start_live(task, &cwd, ctx, cancel, on_event),
        }
    }
}

impl CliWorker {
    /// Start a live (claude/codex) session: build the approval proxy from
    /// `ctx.approver`, spawn the persistent driver + run turn 1, stream progress →
    /// [`WorkerEvent::Progress`], then synthesize the canonical step stream.
    fn start_live(
        &self,
        task: &WorkerTask,
        cwd: &std::path::Path,
        ctx: &WorkerContext,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let proxy = self.build_proxy(ctx);
        let model = task.model.clone();
        let mut on_progress = |text: &str| {
            on_event(WorkerEvent::Progress {
                text: text.to_string(),
            })
        };
        let (driver, turn) = self
            .spawn_live_driver(
                cwd,
                task.autonomy,
                model,
                &task.prompt,
                proxy,
                cancel,
                &mut on_progress,
            )
            .map_err(|err| {
                if cancel.is_cancelled() {
                    WorkerError::Cancelled
                } else {
                    WorkerError::Start(err.to_string())
                }
            })?;
        let result = turn_to_result(&turn);
        synthesize_turn_steps(1, &result, on_event);
        Ok(Box::new(LiveCliSession {
            driver: Mutex::new(Some(driver)),
            session_cancel: CancelToken::new(),
            last: Mutex::new(result),
        }))
    }

    /// Spawn the right persistent driver for the agent and run turn 1. Mirrors the
    /// daemon's `start_live_driver` recipe (the lowest-risk reuse) without touching
    /// it: it calls the SAME [`DelegateSession::start`] / [`CodexSession::start`].
    #[allow(clippy::too_many_arguments)] // reason: one cohesive spawn call mirroring
    // the daemon's `start_live_driver`; cwd, autonomy, model, the first message, the
    // proxy, and the cancel/progress sinks are independent spawn inputs and a struct
    // would add indirection without isolating a responsibility.
    fn spawn_live_driver(
        &self,
        cwd: &std::path::Path,
        autonomy: DelegateAutonomy,
        model: Option<String>,
        first_message: &str,
        proxy: Option<DelegateProxy>,
        cancel: &CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<
        (LiveDriver, crate::delegate_session::TurnResult),
        crate::delegate_session::SessionError,
    > {
        let launcher = self.launcher.as_ref();
        match self.agent {
            DelegateAgent::Codex => {
                let (session, turn) = crate::delegate_session_codex::CodexSession::start(
                    launcher,
                    cwd,
                    autonomy,
                    model.as_deref(),
                    first_message,
                    proxy,
                    &self.mcp_disable_flags,
                    cancel,
                    on_progress,
                )?;
                Ok((LiveDriver::Codex(session), turn))
            }
            _ => {
                let (session, turn) = crate::delegate_session::DelegateSession::start(
                    launcher,
                    cwd,
                    autonomy,
                    model.as_deref(),
                    first_message,
                    proxy,
                    cancel,
                    on_progress,
                )?;
                Ok((LiveDriver::Claude(session), turn))
            }
        }
    }

    /// Start `gemini` one-shot through the streaming launcher + [`DelegateParser`]
    /// (reusing the DA-2 recipe), projecting each parsed line → progress and the
    /// final outcome → the synthesized step stream.
    fn start_gemini(
        &self,
        task: &WorkerTask,
        cwd: &std::path::Path,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        let invocation = delegate_runtime::build_command(
            DelegateAgent::Gemini,
            &task.prompt,
            cwd,
            task.autonomy,
            task.model.as_deref(),
            &self.mcp_disable_flags,
        );
        let policy = delegate_runtime::delegate_policy(cwd);
        let mut parser = DelegateParser::new(DelegateAgent::Gemini);
        let mut on_line = |line: &str| {
            if let Some(text) = parser.ingest(line) {
                on_event(WorkerEvent::Progress { text });
            }
        };
        let output = self
            .launcher
            .launch_streaming(
                &invocation.spec,
                &policy,
                &invocation.stdin,
                cancel,
                &mut on_line,
            )
            .map_err(|err| {
                if cancel.is_cancelled() {
                    WorkerError::Cancelled
                } else {
                    WorkerError::Start(err.to_string())
                }
            })?;
        if cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let outcome = parser.finish("gemini", output.exit_code, output.timed_out);
        let result = outcome_to_result(&outcome);
        synthesize_turn_steps(1, &result, on_event);
        Ok(Box::new(OneShotCliSession { last: result }))
    }

    /// Build the proxied-mode approval bridge from `ctx.approver`, wrapping it so a
    /// `can_use_tool` ask is also re-projected as a [`WorkerEvent::Approval`] before
    /// the real round-trip drives the operator decision. We always proxy so
    /// approvals route through the one hub.
    ///
    /// The projection is recorded into `ctx.ledger` (the thread-safe, kind-agnostic
    /// record) rather than the turn's `&mut on_event` sink: the proxy resolves on
    /// the driver's read-loop control flow, so threading the borrow there would
    /// require cross-thread event plumbing that C1 wires when the engine owns the
    /// node-scoped sink. The blocking operator round-trip is UNCHANGED.
    fn build_proxy(&self, ctx: &WorkerContext) -> Option<DelegateProxy> {
        let agent = self.agent.catalog_name();
        let approver: Arc<dyn DelegateApprover> = Arc::new(ProjectingApprover {
            inner: Arc::clone(&ctx.approver),
            sink: Arc::clone(&ctx.ledger),
            node_id: format!("cli-{agent}"),
            seq: AtomicU64::new(0),
        });
        Some(DelegateProxy::for_agent(
            approver,
            // The approval is keyed under the agent name for C0 (the engine assigns
            // node ids in C1); the hub mints its own request_id.
            format!("cli-{agent}"),
            DelegateDecisions::default(),
            agent,
        ))
    }
}

/// An approver that re-projects each `can_use_tool` ask as a [`WorkerEvent::Approval`]
/// into the [`WorkerLedger`](super::WorkerLedger) (the kind-agnostic record), then
/// delegates to the real hub for the blocking operator round-trip. The decision is
/// UNCHANGED — this only adds the projection so the engine/clients see the ask.
struct ProjectingApprover {
    inner: Arc<dyn DelegateApprover>,
    sink: Arc<super::WorkerLedger>,
    node_id: String,
    seq: AtomicU64,
}

impl DelegateApprover for ProjectingApprover {
    fn request(
        &self,
        session_id: &str,
        tool: &str,
        args: &Value,
        tier: RiskTier,
        preview: String,
        cancel: &CancelToken,
    ) -> SessionApprovalDecision {
        // The hub mints the real request_id; the projection uses a per-worker seq
        // as a stable local id (the round-trip below is what actually resolves it).
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        self.sink.record_event(
            &self.node_id,
            WorkerEvent::Approval {
                request_id: format!("{session_id}-{n}"),
                tool: tool.to_string(),
                args: args.clone(),
                tier,
                preview: preview.clone(),
            },
        );
        self.inner
            .request(session_id, tool, args, tier, preview, cancel)
    }
}

/// A live claude/codex worker session over a [`LiveDriver`]. Steering runs another
/// turn under a token linked to the session-scoped cancel; close reaps the child.
struct LiveCliSession {
    /// `None` once closed/reaped, so a late steer sees a clear closed error.
    driver: Mutex<Option<LiveDriver>>,
    /// Session-scoped cancel fired by [`Self::interrupt`]/[`Self::close`].
    session_cancel: CancelToken,
    last: Mutex<TurnResult>,
}

impl WorkerSession for LiveCliSession {
    fn steer(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        let mut guard = crate::sync::lock_recover(&self.driver);
        let driver = guard
            .as_mut()
            .ok_or_else(|| WorkerError::Turn("delegated session is already closed".into()))?;
        // Honor both the per-turn cancel and the session-scoped cancel.
        if cancel.is_cancelled() || self.session_cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let mut on_progress = |text: &str| {
            on_event(WorkerEvent::Progress {
                text: text.to_string(),
            })
        };
        // Match the `LiveDriver` enum here and call each session's `pub(crate)`
        // `steer` directly, rather than the private `LiveDriver::steer`, so this
        // reuse touches nothing in `delegate_live`.
        let result = match driver {
            LiveDriver::Claude(session) => session.steer(message, cancel, &mut on_progress),
            LiveDriver::Codex(session) => session.steer(message, cancel, &mut on_progress),
        }
        .map_err(|err| match err {
            crate::delegate_session::SessionError::Cancelled => WorkerError::Cancelled,
            other => WorkerError::Turn(other.to_string()),
        });
        match result {
            Ok(turn) => {
                let result = turn_to_result(&turn);
                synthesize_turn_steps(2, &result, on_event);
                *crate::sync::lock_recover(&self.last) = result.clone();
                Ok(result)
            }
            Err(WorkerError::Cancelled) => {
                // A cancelled turn leaves the session half-consumed: reap it so a
                // later steer sees "closed" rather than reading undrained lines.
                if let Some(mut driver) = guard.take() {
                    driver.close();
                }
                Err(WorkerError::Cancelled)
            }
            Err(other) => Err(other),
        }
    }

    fn interrupt(&self) {
        self.session_cancel.cancel();
    }

    fn close(&mut self) {
        self.session_cancel.cancel();
        if let Some(mut driver) = crate::sync::lock_recover(&self.driver).take() {
            driver.close();
        }
    }

    fn result(&self) -> TurnResult {
        crate::sync::lock_recover(&self.last).clone()
    }
}

/// A one-shot (gemini) worker session: turn 1 already ran in `start`, so there is
/// nothing live to steer or close.
struct OneShotCliSession {
    last: TurnResult,
}

impl WorkerSession for OneShotCliSession {
    fn steer(
        &mut self,
        _message: &str,
        _cancel: &CancelToken,
        _on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        Err(WorkerError::NotSteerable)
    }

    fn interrupt(&self) {}

    fn close(&mut self) {}

    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

/// Map a live-driver [`TurnResult`](crate::delegate_session::TurnResult) into the
/// port's [`TurnResult`], converting the per-agent [`DelegateUsage`] into the
/// shared [`nerve_agent::Usage`].
fn turn_to_result(turn: &crate::delegate_session::TurnResult) -> TurnResult {
    TurnResult {
        ok: turn.ok,
        text: turn.result.clone(),
        usage: delegate_usage_to_usage(turn.usage),
        cost_usd: turn.cost_usd,
        timed_out: false,
    }
}

/// Map a one-shot [`DelegateOutcome`] into the port's [`TurnResult`].
fn outcome_to_result(outcome: &DelegateOutcome) -> TurnResult {
    TurnResult {
        ok: outcome.ok,
        text: outcome.result.clone(),
        usage: delegate_usage_to_usage(outcome.usage),
        cost_usd: outcome.cost_usd,
        timed_out: outcome.timed_out,
    }
}

/// Convert a [`DelegateUsage`] (the CLI stream's parsed tokens) into the shared
/// [`nerve_agent::Usage`]. Counts are saturated into `u32` (the agent type's
/// width); a delegated run's counts comfortably fit, and saturation is fail-safe.
fn delegate_usage_to_usage(usage: Option<DelegateUsage>) -> nerve_agent::Usage {
    let usage = usage.unwrap_or_default();
    nerve_agent::Usage {
        input_tokens: usage.input_tokens.min(u64::from(u32::MAX)) as u32,
        output_tokens: usage.output_tokens.min(u64::from(u32::MAX)) as u32,
        cache_read_tokens: usage.cache_read_tokens.min(u64::from(u32::MAX)) as u32,
        cache_creation_tokens: usage.cache_creation_tokens.min(u64::from(u32::MAX)) as u32,
    }
}
