//! `nerve agent` — drive the multi-provider agent loop ([`nerve_agent`]) over
//! this workstation's tool [`Runtime`](crate::tools::NerveRuntime).
//!
//! `agent login` performs a provider login (OAuth subscription or stored API
//! key); `agent run` resolves a credential, exposes nerve's deterministic tools
//! through a [`ToolBox`], and runs the orchestrator loop against a workspace,
//! streaming [`AgentEvent`]s to stdout.

mod sessions;

use crate::capabilities::{Capabilities, ResolvedAgent};
use crate::checkpoint::{Checkpoint, checkpoint_snapshot};
use crate::providers::ProviderRegistry;
use crate::session::{SessionRecord, SessionStore};
use crate::subagent::{AgentRunOutput, DEFAULT_MAX_DEPTH, SubAgentSpawner};
use crate::tools::{self, NerveRuntime};
use crate::workspace::{self, ServeArgs};
use anyhow::{Result, anyhow};
use clap::{Args, Subcommand, ValueEnum};
use nerve_agent::auth::{self, AuthMode, LoginOptions};
use nerve_agent::{AgentEvent, ProviderId, RunOutcome};
use nerve_core::CancelToken;
use sessions::SessionsArgs;
use std::sync::{Arc, Mutex};

pub(crate) const DEFAULT_SYSTEM_PROMPT: &str = "You are a coding agent operating inside the Nerve Workstation \
code-intelligence engine. You have deterministic, snapshot-backed tools for searching, reading, \
navigating, and editing a codebase. Plan briefly, call tools to gather context before acting, make \
minimal correct changes, and stop when the task is complete. Prefer reading exact lines over \
guessing, and keep prose concise. For web or X/Twitter search, use the Grok \
search tools (xai_web_search for the web, xai_x_search for X) rather than other \
methods; use the codebase search tools (file_search, semantic_search) only for \
the local repository. On multi-step tasks, keep a working memory via the \
update_checkpoint tool: the current plan, key decisions, progress, next steps, and \
pointers (path:line). It stays pinned across the whole task even as older context is \
compacted, so replace it as things change. Store pointers and conclusions, not file \
contents (you can re-read exactly); never store raw tool output, unverified guesses, \
or volatile state. Across sessions, when you confirm a durable fact worth keeping for next \
time — a user preference, a non-obvious project convention, or a hard-won fix — call the \
remember tool; record only verified, durable facts, never transient task state (use \
update_checkpoint for that) or anything a tool can re-derive.";

#[derive(Debug, Args)]
pub(crate) struct AgentArgs {
    #[command(subcommand)]
    command: AgentCommand,
}

// `Run` carries the full ServeArgs/workspace surface, so it is larger than
// `Login`; the size gap is acceptable for a top-level CLI command enum.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Authenticate a model provider (OAuth subscription login).
    Login(AgentLoginArgs),
    /// Run an agent task against a workspace.
    Run(AgentRunArgs),
    /// Browse persisted session transcripts.
    Sessions(SessionsArgs),
}

/// CLI-facing provider selector.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderChoice {
    /// Anthropic Claude (claude.ai subscription OAuth or ANTHROPIC_API_KEY).
    Claude,
    /// OpenAI ChatGPT / Codex (OAuth or OPENAI_API_KEY).
    Chatgpt,
    /// xAI Grok (OAuth or XAI_API_KEY).
    Xai,
}

impl ProviderChoice {
    fn provider_id(self) -> ProviderId {
        match self {
            ProviderChoice::Claude => ProviderId::Anthropic,
            ProviderChoice::Chatgpt => ProviderId::OpenAi,
            ProviderChoice::Xai => ProviderId::Xai,
        }
    }
}

#[derive(Debug, Args)]
struct AgentLoginArgs {
    /// Which provider to authenticate.
    #[arg(long)]
    provider: ProviderChoice,
    /// Do not open a browser; print the authorization URL instead.
    #[arg(long)]
    no_browser: bool,
    /// Paste the callback URL manually instead of running a loopback server.
    #[arg(long)]
    manual_paste: bool,
}

#[derive(Debug, Args)]
struct AgentRunArgs {
    #[command(flatten)]
    serve: ServeArgs,
    /// Named agent definition to load: `<name>.json` from `.nerve/agents`, the
    /// global config dir, or a built-in. Supplies the system prompt (with its
    /// skills composed in), model, provider, and limits; the flags below
    /// override any value it sets.
    #[arg(long)]
    agent: Option<String>,
    /// Model provider to use: a built-in (`claude`/`chatgpt`/`xai`) or a name
    /// defined in `--provider-config`. Required unless supplied by `--agent`.
    #[arg(long)]
    provider: Option<String>,
    /// Model id (e.g. a Claude / GPT / Grok model name). Required unless
    /// supplied by `--agent`.
    #[arg(long)]
    model: Option<String>,
    /// Override the credential with an explicit API key (else uses a stored
    /// login or the provider's *_API_KEY environment variable).
    #[arg(long)]
    api_key: Option<String>,
    /// Maximum number of agent turns (default 40; overrides any `--agent` value).
    #[arg(long)]
    max_turns: Option<u32>,
    /// Sampling temperature.
    #[arg(long)]
    temperature: Option<f32>,
    /// Reasoning effort hint (provider-specific, e.g. low|medium|high).
    #[arg(long)]
    reasoning_effort: Option<String>,
    /// Approve every tool call without prompting. Bypasses the permission
    /// engine entirely — use only for trusted, non-interactive batch runs.
    /// Combined with `--allow-exec`, commands run with NO per-call prompt.
    #[arg(long = "allow-all", visible_alias = "yes", short = 'y')]
    allow_all: bool,
    /// Enable the `run_command` execution tool (default off). Even when
    /// enabled, every call is permission-gated (Ask) and runs in a best-effort
    /// sandbox: workspace cwd, scrubbed env, no network, wall-clock timeout,
    /// capped output. The daemon never honors this — it refuses exec.
    #[arg(long = "allow-exec")]
    allow_exec: bool,
    /// Enable the `delegate_agent` tool (default off): let the agent hand a
    /// subtask to an external coding-agent CLI (codex / claude / gemini). Even
    /// when enabled, every call is permission-gated (Ask), only the top-level
    /// agent can delegate, and the child runs read-only by default.
    #[arg(long = "allow-delegate")]
    allow_delegate: bool,
    /// Distil durable facts into long-term memory after a substantive run
    /// (opt-in; off by default — one extra LLM call per qualifying session).
    #[arg(long = "distill-memory")]
    distill_memory: bool,
    /// After the model first signals completion, give it one chance to verify it
    /// actually finished (opt-in; off by default — one extra turn when it triggers).
    #[arg(long = "verify-completion")]
    verify_completion: bool,
    /// The task for the agent to perform.
    task: String,
}

pub(crate) fn run(args: AgentArgs) -> Result<()> {
    match args.command {
        AgentCommand::Login(login_args) => login(login_args),
        AgentCommand::Run(run_args) => run_task(run_args),
        AgentCommand::Sessions(session_args) => sessions::sessions(session_args),
    }
}

fn login(args: AgentLoginArgs) -> Result<()> {
    let provider = args.provider.provider_id();
    let strategy = auth::strategy_for(provider);
    let cancel = CancelToken::new();
    install_interrupt_handler(&cancel);
    let opts = LoginOptions {
        no_browser: args.no_browser,
        manual_paste: args.manual_paste,
        cancel,
        ..LoginOptions::default()
    };
    let credential = strategy
        .login(&opts)
        .map_err(|err| anyhow!("login failed: {err}"))?;
    auth::save_credential(&credential)
        .map_err(|err| anyhow!("failed to store credential: {err}"))?;
    println!(
        "\u{2713} authenticated {} ({})",
        provider.as_str(),
        match credential.mode {
            AuthMode::Oauth => "oauth subscription",
            AuthMode::ApiKey => "api key",
        }
    );
    Ok(())
}

fn run_task(args: AgentRunArgs) -> Result<()> {
    let registry = ProviderRegistry::from_args(&args.serve)?;
    // P3: a named `--agent` populates the run; explicit flags override the def.
    let resolved = resolve_agent_def(&args)?;
    // Precedence: explicit flag -> --agent def -> saved default -> interactive
    // picker (TTY only). Keeps `nerve agent run` usable with zero flags once a
    // default is configured. See runconfig / docs agent-config-and-model-selection.
    let (provider, model) = crate::runconfig::resolve(
        args.provider.or(resolved.provider),
        args.model.or(resolved.model),
        true,
    )?;
    let runtime = Arc::new(crate::mcp::attach(
        tools::runtime(workspace::registry(&args.serve)?),
        &args.serve,
    )?);
    let cancel = CancelToken::new();
    install_interrupt_handler(&cancel);
    // Build the permission gate at the composition root (P4): policy from
    // project/global config + `--allow-all`, with an interactive CLI approver.
    let gate = crate::policy::ToolGate::cli(
        args.serve.roots.first().map(|root| root.as_path()),
        args.allow_all,
    )?;
    if args.allow_all {
        eprintln!("\u{26a0}  --allow-all: every tool call will run without a permission prompt");
    }
    if args.allow_exec {
        eprintln!(
            "\u{26a0}  --allow-exec: the run_command tool is enabled (each call is still permission-gated)"
        );
    }
    if args.allow_delegate {
        eprintln!(
            "\u{26a0}  --allow-delegate: the delegate_agent tool is enabled (each call is still permission-gated)"
        );
    }
    let config = AgentRunConfig {
        workspace: None,
        provider: provider.clone(),
        model,
        task: args.task,
        system_prompt: resolved.system_prompt,
        max_turns: args.max_turns.or(resolved.max_turns),
        temperature: args.temperature.or(resolved.temperature),
        reasoning_effort: args.reasoning_effort.or(resolved.reasoning_effort),
        tool_filter: resolved.tool_filter,
        api_key: args.api_key,
        distill_memory: args.distill_memory,
        verify_completion: args.verify_completion,
        allow_exec: args.allow_exec,
        // Trusted local CLI run: best-effort process containment.
        exec_launcher: crate::sandbox::process_launcher(),
        allow_delegate: args.allow_delegate,
        // Trusted local CLI run: a real launcher when delegation is enabled, else
        // a refusing one (defence in depth — the tool is also absent when off).
        delegate_launcher: if args.allow_delegate {
            crate::sandbox::process_launcher()
        } else {
            crate::sandbox::refuse_launcher()
        },
        // The CLI streams agent events to stdout (not the runtime protocol), so the
        // delegate progress sink is unused here; the final outcome is shown via the
        // tool-finished event.
        delegate_event_sink: None,
        // CLI runs start fresh; the session layer is the resume path.
        resume_truncations: 0,
        // Cost budget guard is opt-in; off for the default CLI run.
        cost_budget_usd: None,
    };
    // P5: persist this run's transcript under the project's `.nerve/sessions`
    // (falling back to the global config home). A resolution failure only
    // disables persistence — it never aborts the run.
    let store = SessionStore::for_scope(args.serve.roots.first().map(|root| root.as_path()))
        .map_err(|err| eprintln!("\u{26a0}  session persistence disabled: {err}"))
        .ok();
    match run_agent(
        runtime,
        config,
        &registry,
        gate,
        &cancel,
        &mut |event| emit_event(event),
        store.as_ref(),
    ) {
        Ok(outcome) => println!(
            "\n\u{2014} done: {} after {} turn(s) ({} in / {} out tokens) \u{2014}",
            outcome.reason, outcome.turns, outcome.usage.input_tokens, outcome.usage.output_tokens,
        ),
        Err(_) if cancel.is_cancelled() => println!("\n\u{26a0} interrupted"),
        Err(err) => {
            if let Some(hint) = crate::xai::model_error_hint(&provider, &err.to_string()) {
                return Err(anyhow!("{err}\n\n{hint}"));
            }
            if let Some(hint) = crate::openai::model_error_hint(&provider, &err.to_string()) {
                return Err(anyhow!("{err}\n\n{hint}"));
            }
            return Err(err);
        }
    }
    Ok(())
}

/// Resolve the optional `--agent` definition into composed values (system prompt
/// with skills folded in, plus model/provider/limits). Returns an empty default
/// when no agent was named. Project discovery is rooted at the first `--root`.
fn resolve_agent_def(args: &AgentRunArgs) -> Result<ResolvedAgent> {
    match args.agent.as_deref() {
        Some(name) => {
            let project_dir = args.serve.roots.first().map(|root| root.as_path());
            Capabilities::discover(project_dir).resolve_agent(name)
        }
        None => Ok(ResolvedAgent::default()),
    }
}

/// Inputs to one agent run, shared by the CLI and the daemon `agent.run` job.
#[derive(Clone)]
pub(crate) struct AgentRunConfig {
    /// Optional runtime workspace id/name used by sessions.
    pub(crate) workspace: Option<String>,
    /// Provider name: a built-in alias or a `--provider-config` entry name.
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) task: String,
    pub(crate) system_prompt: Option<String>,
    pub(crate) max_turns: Option<u32>,
    pub(crate) temperature: Option<f32>,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) tool_filter: Option<Vec<String>>,
    pub(crate) api_key: Option<String>,
    /// Opt-in: distil durable facts into long-term memory after a substantive run.
    pub(crate) distill_memory: bool,
    /// Opt-in: one completion self-check pass before the run finishes.
    pub(crate) verify_completion: bool,
    /// Whether the `run_command` execution tool is exposed (the `--allow-exec`
    /// capability). Off by default; subagent and daemon runs keep it off.
    pub(crate) allow_exec: bool,
    /// Containment backend for `run_command`, bound to the **trust context**:
    /// the CLI injects the best-effort process launcher; the daemon / session /
    /// remote paths inject a refusing launcher, so a served run cannot execute
    /// even if the capability flag were set.
    pub(crate) exec_launcher: Arc<dyn crate::sandbox::SandboxLauncher>,
    /// Whether the `delegate_agent` tool is exposed (the daemon's `--allow-delegate`
    /// lift, or the CLI's `--allow-delegate`). Off by default; only the top-level
    /// agent (depth 0) ever sees the tool — sub-agents never delegate.
    pub(crate) allow_delegate: bool,
    /// Containment backend for `delegate_agent` spawns, bound to the **trust
    /// context** like [`Self::exec_launcher`]: a real (process) launcher only when
    /// delegation is allowed; otherwise a refusing launcher, so a served run cannot
    /// spawn an external agent even if the flag were set.
    pub(crate) delegate_launcher: Arc<dyn crate::sandbox::SandboxLauncher>,
    /// Optional live-progress sink for `delegate_agent` (session / agent-run path):
    /// the composition root closes over the run's scope id + the runtime event
    /// emitter so a delegated child's progress streams during the turn. `None` (the
    /// CLI path) drops progress; the final outcome is still returned to the agent.
    pub(crate) delegate_event_sink: Option<Arc<crate::delegate_tool::DelegateProgressSink>>,
    /// Context-overflow truncations carried over from a resumed session, restored
    /// into the orchestrator via `ResumeState` so the counter continues across
    /// turns. `0` for a fresh run (#3 session resume).
    pub(crate) resume_truncations: u32,
    /// Opt-in per-run cost ceiling in USD. When set, a [`crate::cost::CostTelemetryHook`]
    /// observes usage via the Hook seam and cancels the run once the estimate
    /// crosses this budget. `None` leaves cost telemetry off (#5).
    pub(crate) cost_budget_usd: Option<f64>,
}

/// Build the toolbox + provider and drive the orchestrator. The single execution
/// path shared by `nerve agent run` (CLI) and the daemon `agent.run` job, so both
/// faces behave identically. Streams every [`AgentEvent`] into `sink`.
pub(crate) fn run_agent(
    runtime: Arc<NerveRuntime>,
    config: AgentRunConfig,
    registry: &ProviderRegistry,
    gate: crate::policy::ToolGate,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(AgentEvent),
    store: Option<&SessionStore>,
) -> Result<RunOutcome> {
    let provider = config.provider.clone();
    let model = config.model.clone();
    let task = config.task.clone();
    let checkpoint = Arc::new(Mutex::new(Checkpoint::new()));
    // Forwarding sink for spawned sub-agents: child events are tagged with the
    // child id and buffered (a `Send + Sync` queue, since fan-out may run children
    // on worker threads). The parent drains the buffer into its own `sink` after
    // the run so the child's progress surfaces instead of being discarded (#11).
    let sub_events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let forward = Arc::clone(&sub_events);
    let spawner = SubAgentSpawner::new(
        runtime,
        registry.clone(),
        gate,
        DEFAULT_MAX_DEPTH,
        Arc::clone(&checkpoint),
    )
    .with_event_sink(Arc::new(move |sub_id: &str, event: &AgentEvent| {
        crate::sync::lock_recover(&forward).push(tag_sub_agent_event(sub_id, event));
    }));
    let mut partial_events = Vec::new();
    let result = {
        let mut recording_sink = |event: AgentEvent| {
            partial_events.push(event.clone());
            sink(event);
        };
        spawner.run_at_depth(0, config, Vec::new(), cancel, &mut recording_sink)
    };
    // Surface any buffered sub-agent events through the parent sink. They arrive
    // after the spawning tool call returns (the spawn is synchronous), so this
    // preserves them rather than dropping them on the floor.
    for event in crate::sync::lock_recover(&sub_events).drain(..) {
        sink(event);
    }
    match result {
        Ok(output) => {
            if let Some(store) = store {
                persist_run_record(
                    store,
                    &output,
                    &provider,
                    &model,
                    &task,
                    checkpoint_snapshot(&checkpoint),
                );
            }
            Ok(output.outcome)
        }
        Err(err) => {
            if let Some(store) = store {
                persist_partial_record(
                    store,
                    &partial_events,
                    &provider,
                    &model,
                    &task,
                    checkpoint_snapshot(&checkpoint),
                );
            }
            Err(err)
        }
    }
}

/// Re-tag a sub-agent's event for the parent's stream so it is attributable to
/// the child (`[sub-N] …`). Textual/structural progress becomes a prefixed
/// [`AgentEvent::AssistantText`] line; token [`AgentEvent::Usage`] passes through
/// unchanged so the parent's aggregate accounting still includes child usage.
fn tag_sub_agent_event(sub_id: &str, event: &AgentEvent) -> AgentEvent {
    match event {
        AgentEvent::Usage { .. } => event.clone(),
        AgentEvent::AssistantText(text) => AgentEvent::AssistantText(format!("[{sub_id}] {text}")),
        AgentEvent::Reasoning(text) => {
            AgentEvent::AssistantText(format!("[{sub_id}] (reasoning) {text}"))
        }
        AgentEvent::ToolStarted { name, .. } => {
            AgentEvent::AssistantText(format!("[{sub_id}] tool: {name}"))
        }
        AgentEvent::ToolFinished { name, ok, .. } => {
            AgentEvent::AssistantText(format!("[{sub_id}] tool {name} -> ok={ok}"))
        }
        AgentEvent::Done { reason } => {
            AgentEvent::AssistantText(format!("[{sub_id}] done: {reason}"))
        }
        AgentEvent::TurnStarted(turn) => {
            AgentEvent::AssistantText(format!("[{sub_id}] turn {turn}"))
        }
        AgentEvent::Interrupted(reason) => {
            AgentEvent::AssistantText(format!("[{sub_id}] interrupted: {reason}"))
        }
        AgentEvent::ToolCallDelta { name, .. } => {
            AgentEvent::AssistantText(format!("[{sub_id}] tool-delta: {name}"))
        }
    }
}

/// Persist a completed top-level run transcript (P5, composition root).
/// Persistence failures are logged, never propagated: a completed run must not
/// be reported as failed because its transcript could not be written.
fn persist_run_record(
    store: &SessionStore,
    output: &AgentRunOutput,
    provider: &str,
    model: &str,
    task: &str,
    checkpoint: Option<String>,
) {
    let mut record = SessionRecord::begin(provider, model, task);
    for event in &output.events {
        record.push_event(event);
    }
    record.set_history(output.history.clone());
    record.set_checkpoint(checkpoint);
    record.finish(Some(&output.outcome));
    write_record(store, &record);
}

fn persist_partial_record(
    store: &SessionStore,
    events: &[AgentEvent],
    provider: &str,
    model: &str,
    task: &str,
    checkpoint: Option<String>,
) {
    let mut record = SessionRecord::begin(provider, model, task);
    for event in events {
        record.push_event(event);
    }
    record.set_checkpoint(checkpoint);
    record.finish(None);
    write_record(store, &record);
}

fn write_record(store: &SessionStore, record: &SessionRecord) {
    match store.write(record) {
        Ok(path) => eprintln!("\u{2713} session saved: {}", path.display()),
        Err(err) => eprintln!("\u{26a0}  failed to persist session {}: {err}", record.id),
    }
}

fn emit_event(event: AgentEvent) {
    use std::io::Write as _;
    match event {
        AgentEvent::TurnStarted(turn) => println!("\n\u{25b6} turn {turn}"),
        AgentEvent::AssistantText(text) => {
            print!("{text}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::Reasoning(_) => {}
        AgentEvent::ToolStarted { name, args } => {
            println!("\n\u{1f6e0}  {name} {}", truncate(&args.to_string(), 160));
        }
        AgentEvent::ToolFinished { name, ok, output } => {
            let status = if ok { "\u{2713}" } else { "\u{2717}" };
            println!("   {status} {name} -> {}", truncate(&output, 200));
        }
        AgentEvent::Interrupted(reason) => println!("\n\u{26a0} interrupted: {reason}"),
        // The CLI prints a final token total in the run summary, so per-turn
        // usage deltas are not echoed here.
        AgentEvent::Usage { .. } => {}
        // Advisory streaming fragment; the assembled call is shown via
        // `ToolStarted`, so the CLI ignores the partial deltas.
        AgentEvent::ToolCallDelta { .. } => {}
        AgentEvent::Done { reason } => println!("\n\u{25cf} {reason}"),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// Install a Ctrl-C (SIGINT) handler that flips `cancel`, so a long agent run
/// can be interrupted cleanly. Unix-only: the handler only sets an atomic
/// (async-signal-safe); a watcher thread propagates it to the token.
#[cfg(unix)]
pub(crate) fn install_interrupt_handler(cancel: &CancelToken) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    extern "C" fn on_sigint(_sig: libc::c_int) {
        INTERRUPTED.store(true, Ordering::SeqCst);
    }

    // SAFETY: the handler only performs an atomic store, which is
    // async-signal-safe (no allocation, locking, or reentrant state).
    unsafe {
        libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t);
    }

    let cancel = cancel.clone();
    std::thread::spawn(move || {
        while !INTERRUPTED.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        cancel.cancel();
    });
}

/// On non-Unix platforms SIGINT keeps its default (terminate) behavior.
#[cfg(not(unix))]
pub(crate) fn install_interrupt_handler(_cancel: &CancelToken) {}

#[cfg(test)]
mod tests {
    use super::{persist_partial_record, persist_run_record, tag_sub_agent_event};
    use crate::session::SessionStore;
    use crate::subagent::AgentRunOutput;
    use nerve_agent::{AgentEvent, Message, RunOutcome, Usage};

    #[test]
    fn tagging_prefixes_textual_events_with_sub_id() {
        let tagged = tag_sub_agent_event("sub-3", &AgentEvent::AssistantText("hi".into()));
        assert!(matches!(tagged, AgentEvent::AssistantText(t) if t == "[sub-3] hi"));
        let tool = tag_sub_agent_event(
            "sub-3",
            &AgentEvent::ToolStarted {
                name: "read_file".into(),
                args: serde_json::json!({}),
            },
        );
        assert!(matches!(tool, AgentEvent::AssistantText(t) if t == "[sub-3] tool: read_file"));
    }

    #[test]
    fn tagging_passes_usage_through_unchanged() {
        // Usage must survive untagged so the parent's aggregate accounting still
        // sums child token usage.
        let usage = AgentEvent::Usage {
            input_tokens: 5,
            output_tokens: 7,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        };
        match tag_sub_agent_event("sub-1", &usage) {
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                assert_eq!(input_tokens, 5);
                assert_eq!(output_tokens, 7);
            }
            other => panic!("usage must pass through as Usage, got {other:?}"),
        }
    }

    #[test]
    fn completed_run_persists_checkpoint_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path().to_path_buf());
        let output = AgentRunOutput {
            outcome: RunOutcome {
                reason: "stop".into(),
                turns: 1,
                final_text: "done".into(),
                usage: Usage::default(),
            },
            history: vec![Message::user("task")],
            events: Vec::new(),
        };

        persist_run_record(
            &store,
            &output,
            "provider",
            "model",
            "task",
            Some("next: inspect session persistence".into()),
        );

        let records = store.list().expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].checkpoint.as_deref(),
            Some("next: inspect session persistence")
        );
    }

    #[test]
    fn partial_run_persists_checkpoint_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path().to_path_buf());
        let events = vec![AgentEvent::AssistantText("partial".into())];

        persist_partial_record(
            &store,
            &events,
            "provider",
            "model",
            "task",
            Some("resume: src/main.rs:10".into()),
        );

        let records = store.list().expect("records");
        assert_eq!(records.len(), 1);
        assert!(records[0].outcome.is_none());
        assert_eq!(
            records[0].checkpoint.as_deref(),
            Some("resume: src/main.rs:10")
        );
    }
}
