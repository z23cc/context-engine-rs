//! `nerve agent` — drive the multi-provider agent loop ([`nerve_agent`]) over
//! this workstation's tool [`Runtime`](crate::tools::NerveRuntime).
//!
//! `agent login` performs a provider login (OAuth subscription or stored API
//! key); `agent run` resolves a credential, exposes nerve's deterministic tools
//! through a [`ToolBox`], and runs the orchestrator loop against a workspace,
//! streaming [`AgentEvent`]s to stdout.

use crate::capabilities::{Capabilities, ResolvedAgent};
use crate::checkpoint::Checkpoint;
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
use std::path::PathBuf;
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
    #[arg(long = "allow-all", visible_alias = "yes", short = 'y')]
    allow_all: bool,
    /// Distil durable facts into long-term memory after a substantive run
    /// (opt-in; off by default — one extra LLM call per qualifying session).
    #[arg(long = "distill-memory")]
    distill_memory: bool,
    /// The task for the agent to perform.
    task: String,
}

#[derive(Debug, Args)]
struct SessionsArgs {
    #[command(subcommand)]
    command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List recent agent sessions, most recent first.
    List(SessionsScopeArgs),
    /// Print a stored session transcript.
    Show(SessionsShowArgs),
}

#[derive(Debug, Args)]
struct SessionsScopeArgs {
    /// Project root whose `.nerve/sessions` is read. Defaults to the current
    /// directory; pass the same `--root` you ran the agent with.
    #[arg(long = "root")]
    root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SessionsShowArgs {
    #[command(flatten)]
    scope: SessionsScopeArgs,
    /// Session id, as shown by `nerve agent sessions list`.
    id: String,
    /// Print the raw stored JSON instead of a formatted transcript.
    #[arg(long)]
    json: bool,
}

fn sessions(args: SessionsArgs) -> Result<()> {
    match args.command {
        SessionsCommand::List(scope) => sessions_list(&scope),
        SessionsCommand::Show(show) => sessions_show(&show),
    }
}

/// Resolve the session store for a browse scope. `--root` defaults to the current
/// directory so `sessions list` works from inside a project; with neither a root
/// nor a usable current directory, the global config home is used.
fn sessions_store(scope: &SessionsScopeArgs) -> Result<SessionStore> {
    let root = scope.root.clone().or_else(|| std::env::current_dir().ok());
    SessionStore::for_scope(root.as_deref())
}

fn sessions_list(scope: &SessionsScopeArgs) -> Result<()> {
    let store = sessions_store(scope)?;
    let records = store.list()?;
    if records.is_empty() {
        println!("no sessions in {}", store.dir().display());
        return Ok(());
    }
    for record in &records {
        println!("{}", record.summary_line());
    }
    Ok(())
}

fn sessions_show(args: &SessionsShowArgs) -> Result<()> {
    let store = sessions_store(&args.scope)?;
    if args.json {
        println!("{}", store.read_raw(&args.id)?);
    } else {
        print!("{}", store.load(&args.id)?.render_transcript());
    }
    Ok(())
}

pub(crate) fn run(args: AgentArgs) -> Result<()> {
    match args.command {
        AgentCommand::Login(login_args) => login(login_args),
        AgentCommand::Run(run_args) => run_task(run_args),
        AgentCommand::Sessions(session_args) => sessions(session_args),
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
    let provider = args.provider.or(resolved.provider).ok_or_else(|| {
        anyhow!("no provider: pass --provider or use --agent NAME that defines one")
    })?;
    let model = args
        .model
        .or(resolved.model)
        .ok_or_else(|| anyhow!("no model: pass --model or use --agent NAME that defines one"))?;
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
    let spawner = SubAgentSpawner::new(
        runtime,
        registry.clone(),
        gate,
        DEFAULT_MAX_DEPTH,
        Arc::clone(&checkpoint),
    );
    let mut partial_events = Vec::new();
    let result = {
        let mut recording_sink = |event: AgentEvent| {
            partial_events.push(event.clone());
            sink(event);
        };
        spawner.run_at_depth(0, config, Vec::new(), cancel, &mut recording_sink)
    };
    match result {
        Ok(output) => {
            if let Some(store) = store {
                persist_run_record(store, &output, &provider, &model, &task);
            }
            Ok(output.outcome)
        }
        Err(err) => {
            if let Some(store) = store {
                persist_partial_record(store, &partial_events, &provider, &model, &task);
            }
            Err(err)
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
) {
    let mut record = SessionRecord::begin(provider, model, task);
    for event in &output.events {
        record.push_event(event);
    }
    record.set_history(output.history.clone());
    record.finish(Some(&output.outcome));
    write_record(store, &record);
}

fn persist_partial_record(
    store: &SessionStore,
    events: &[AgentEvent],
    provider: &str,
    model: &str,
    task: &str,
) {
    let mut record = SessionRecord::begin(provider, model, task);
    for event in events {
        record.push_event(event);
    }
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
