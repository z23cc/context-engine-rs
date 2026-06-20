//! C0 golden parity test — "the property the port buys" (design §2).
//!
//! Drives a [`CliWorker`](super::CliWorker) (backed by a FAKE recording launcher,
//! the same `/bin/sh` fake-claude the daemon delegate tests use) AND the provider
//! event path (backed by a fake [`LlmProvider`], driven through the SAME
//! `agent_event_kind` mapper [`ProviderWorker`](super::ProviderWorker) uses) over
//! the SAME canned task, and asserts BOTH emit the same SHAPE of [`WorkerEvent`]
//! `Step` stream: TurnStarted → Message → Usage. That shared shape — over two
//! unrelated substrates — is the parity the unified port exists to provide.
//!
//! Honest scope note: the CLI side runs end-to-end through the real persistent
//! `DelegateSession` over a contained subprocess. The provider side drives a real
//! [`Orchestrator`] with a fake provider through the EXACT mapper `ProviderWorker`
//! uses (`crate::agent_event::agent_event_kind`); it does not go through
//! `ProviderRegistry::resolve` (which builds a network HTTP provider), so the fake
//! provider is injected at the orchestrator seam rather than the registry. The
//! mapping under test — `AgentEvent → WorkerEvent::Step` — is identical to the
//! production path.

// The fake-claude fixture is a `/bin/sh` script, so the CLI side is unix-only.
#![cfg(unix)]

use super::{
    AgentWorker, BudgetGrant, WorkerContext, WorkerEvent, WorkerLedger, WorkerTask,
    synthesize_turn_steps,
};
use crate::delegate_proxy::DelegateApprover;
use crate::delegate_runtime::DelegateAgent;
use crate::worker::CliWorker;
use nerve_agent::{
    AgentDef, AgentEvent, ChatDelta, ChatRequest, ChatResponse, FinishReason, LlmProvider,
    Orchestrator, ProviderId, ToolBox, ToolSpec, Usage,
};
use nerve_core::CancelToken;
use nerve_runtime::{AgentEventKind, RiskTier, SessionApprovalDecision};
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

/// The canned task both families run.
const CANNED_TASK: &str = "summarize the change";
/// The canned final answer both families produce (so the `Message` step matches).
const CANNED_ANSWER: &str = "the change adds a worker port";

/// A coarse, payload-free fingerprint of a `Step` event, so two substrates that
/// produce the same *structure* (but never identical text) compare equal. Raw
/// `Progress` lines and `Approval` projections are excluded — the parity claim is
/// about the structured `Step` vocabulary.
#[derive(Debug, PartialEq, Eq)]
enum StepShape {
    TurnStarted,
    Message,
    Reasoning,
    ToolStarted,
    ToolFinished,
    Interrupted,
    Usage,
}

/// Reduce a worker's event stream to the canonical `Step` shapes, dropping
/// `Progress`/`Approval` (non-structured) events.
fn step_shapes(events: &[WorkerEvent]) -> Vec<StepShape> {
    events
        .iter()
        .filter_map(|event| match event {
            WorkerEvent::Step(kind) => Some(shape_of(kind)),
            WorkerEvent::Progress { .. } | WorkerEvent::Approval { .. } => None,
        })
        .collect()
}

fn shape_of(kind: &AgentEventKind) -> StepShape {
    match kind {
        AgentEventKind::TurnStarted { .. } => StepShape::TurnStarted,
        AgentEventKind::Message { .. } => StepShape::Message,
        AgentEventKind::Reasoning { .. } => StepShape::Reasoning,
        AgentEventKind::ToolStarted { .. } => StepShape::ToolStarted,
        AgentEventKind::ToolFinished { .. } => StepShape::ToolFinished,
        AgentEventKind::Interrupted { .. } => StepShape::Interrupted,
        AgentEventKind::Usage { .. } => StepShape::Usage,
    }
}

// ---- CLI side: a real DelegateSession over a fake-claude subprocess -----------

/// A fake-claude that emits one assistant line + a result line per user message
/// (the verified stream-json shape), staying alive until stdin EOF. Mirrors the
/// daemon delegate-session test fixture, parameterised with the canned answer.
fn fake_claude_script() -> String {
    format!(
        r#"#!/bin/sh
printf '{{"type":"system","subtype":"init","session_id":"parity-sess"}}\n'
while IFS= read -r line; do
  printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{answer}"}}]}}}}\n'
  printf '{{"type":"result","subtype":"success","is_error":false,"result":"{answer}","session_id":"parity-sess","num_turns":1,"total_cost_usd":0.002,"usage":{{"input_tokens":12,"output_tokens":8}}}}\n'
done
"#,
        answer = CANNED_ANSWER
    )
}

/// A launcher that rewrites the requested `claude` program to the fake script and
/// keeps the real containment policy — so the CLI worker runs a real contained
/// subprocess speaking the protocol (the "recording launcher" the task asks for).
struct FakeClaudeLauncher {
    _dir: tempfile::TempDir,
    script: std::path::PathBuf,
}

impl FakeClaudeLauncher {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-claude.sh");
        std::fs::write(&script, fake_claude_script()).expect("write fake claude");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake claude");
        Arc::new(Self { _dir: dir, script })
    }
}

impl crate::sandbox::SandboxLauncher for FakeClaudeLauncher {
    fn launch(
        &self,
        _spec: &crate::sandbox::CommandSpec,
        _policy: &crate::sandbox::SandboxPolicy,
        _cancel: &CancelToken,
    ) -> anyhow::Result<crate::sandbox::Output> {
        anyhow::bail!("fake claude launcher only supports the persistent path")
    }

    fn launch_persistent(
        &self,
        _spec: &crate::sandbox::CommandSpec,
        policy: &crate::sandbox::SandboxPolicy,
    ) -> anyhow::Result<crate::sandbox::PersistentChild> {
        let spec = crate::sandbox::CommandSpec {
            command: self.script.display().to_string(),
            args: Vec::new(),
        };
        crate::sandbox::PersistentChild::spawn(&spec, policy)
    }
}

/// An approver that never asks (no `can_use_tool` in the canned run); a deny is the
/// safe default if it were ever consulted.
struct SilentApprover;

impl DelegateApprover for SilentApprover {
    fn request(
        &self,
        _session_id: &str,
        _tool: &str,
        _args: &serde_json::Value,
        _tier: RiskTier,
        _preview: String,
        _cancel: &CancelToken,
    ) -> SessionApprovalDecision {
        SessionApprovalDecision::Deny
    }
}

/// Run the canned task through a real [`CliWorker`] over the fake launcher and
/// collect its full `WorkerEvent` stream (turn 1).
fn cli_worker_events() -> Vec<WorkerEvent> {
    let root = tempfile::tempdir().expect("root");
    let worker = CliWorker::new(DelegateAgent::Claude, FakeClaudeLauncher::new(), Vec::new());
    let ctx = WorkerContext {
        root: Some(root.path().to_path_buf()),
        snapshot_generation: 0,
        ledger: Arc::new(WorkerLedger::new()),
        approver: Arc::new(SilentApprover),
    };
    let cancel = CancelToken::never();
    let mut events = Vec::new();
    let mut session = worker
        .start(&canned_task(), &ctx, &cancel, &mut |event| {
            events.push(event)
        })
        .expect("cli worker starts turn 1");
    session.close();
    events
}

// ---- Provider side: a real Orchestrator over a fake LlmProvider ---------------

/// A fake provider returning the canned answer and usage in one turn (FinishReason
/// `Stop` so the orchestrator completes after one response).
struct FakeProvider;

impl LlmProvider for FakeProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    fn chat(
        &self,
        _req: &ChatRequest,
        _cancel: &CancelToken,
        sink: &mut dyn FnMut(ChatDelta),
    ) -> nerve_agent::AgentResult<ChatResponse> {
        sink(ChatDelta::Text(CANNED_ANSWER.to_string()));
        Ok(ChatResponse {
            content: CANNED_ANSWER.to_string(),
            reasoning: None,
            reasoning_signature: None,
            tool_calls: Vec::new(),
            finish_reason: FinishReason::Stop,
            usage: Usage {
                input_tokens: 12,
                output_tokens: 8,
                ..Usage::default()
            },
        })
    }
}

/// An empty toolbox: the canned provider run makes no tool calls.
struct EmptyToolBox;

impl ToolBox for EmptyToolBox {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    fn call(
        &self,
        name: &str,
        _args: &serde_json::Value,
        _cancel: &CancelToken,
    ) -> nerve_agent::AgentResult<serde_json::Value> {
        Err(nerve_agent::AgentError::Tool(format!("no tool `{name}`")))
    }
}

/// Drive a real [`Orchestrator`] with the fake provider and map each [`AgentEvent`]
/// to a [`WorkerEvent::Step`] through the EXACT mapper `ProviderWorker` uses, so the
/// stream is shaped identically to the production provider path.
fn provider_worker_events() -> Vec<WorkerEvent> {
    let provider = FakeProvider;
    let toolbox = EmptyToolBox;
    let def = AgentDef {
        model: "fake-model".into(),
        max_turns: 4,
        ..AgentDef::default()
    };
    let mut orchestrator = Orchestrator::new(&provider, &toolbox, def);
    let cancel = CancelToken::never();
    let mut events = Vec::new();
    orchestrator
        .run(CANNED_TASK, &cancel, &mut |event: AgentEvent| {
            if let Some(kind) = crate::agent_event::agent_event_kind(event) {
                events.push(WorkerEvent::Step(kind));
            }
        })
        .expect("provider run completes");
    events
}

// ---- The parity assertion -----------------------------------------------------

#[test]
fn both_families_emit_the_same_step_shape_for_the_same_task() {
    let cli = step_shapes(&cli_worker_events());
    let provider = step_shapes(&provider_worker_events());

    // The canonical shape the port guarantees, regardless of substrate.
    let canonical = vec![StepShape::TurnStarted, StepShape::Message, StepShape::Usage];

    assert_eq!(cli, canonical, "CLI worker step shape diverged: {cli:?}");
    assert_eq!(
        provider, canonical,
        "provider worker step shape diverged: {provider:?}"
    );
    assert_eq!(
        cli, provider,
        "the two families must emit the SAME step shape (the parity property)"
    );
}

#[test]
fn cli_worker_streams_raw_progress_before_the_synthesized_steps() {
    // The CLI path additionally surfaces the opaque assistant line as a raw
    // `Progress` event (no structured projection) ahead of the synthesized steps —
    // the `Progress` escape hatch the port reserves for opaque CLIs.
    let events = cli_worker_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Progress { text } if text.contains(CANNED_ANSWER))),
        "expected a raw Progress line carrying the assistant text: {events:?}"
    );
    // The provider path never emits a raw Progress line (it is fully structured).
    assert!(
        !provider_worker_events()
            .iter()
            .any(|e| matches!(e, WorkerEvent::Progress { .. }))
    );
}

#[test]
fn synthesized_cli_usage_step_carries_the_parsed_tokens() {
    // The CLI's synthesized Usage step reflects the tokens the fake-claude reported
    // (12 in / 8 out), proving the DelegateUsage → nerve_agent::Usage → Usage step
    // chain is wired, not just shaped.
    let events = cli_worker_events();
    let usage = events
        .iter()
        .find_map(|e| match e {
            WorkerEvent::Step(AgentEventKind::Usage {
                input_tokens,
                output_tokens,
                ..
            }) => Some((*input_tokens, *output_tokens)),
            _ => None,
        })
        .expect("a synthesized Usage step");
    assert_eq!(usage, (12, 8));
}

#[test]
fn synthesize_turn_steps_matches_the_canonical_shape() {
    // A direct unit check that the shared synthesizer (used by the CLI path) emits
    // exactly the canonical TurnStarted → Message → Usage shape.
    let result = super::TurnResult {
        ok: true,
        text: CANNED_ANSWER.to_string(),
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            ..Usage::default()
        },
        cost_usd: None,
        timed_out: false,
    };
    let mut events = Vec::new();
    synthesize_turn_steps(1, &result, &mut |event| events.push(event));
    assert_eq!(
        step_shapes(&events),
        vec![StepShape::TurnStarted, StepShape::Message, StepShape::Usage]
    );
}

fn canned_task() -> WorkerTask {
    WorkerTask {
        prompt: CANNED_TASK.to_string(),
        autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
        model: None,
        tool_filter: None,
        budget: BudgetGrant::default(),
    }
}
