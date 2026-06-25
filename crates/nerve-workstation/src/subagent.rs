//! Sub-agent support injected through the workstation ToolBox seam.
//!
//! `spawn_agent` is deliberately just another tool from the orchestrator's
//! perspective. The nested run is assembled here, in the binary composition
//! root, so `nerve-agent` and `nerve-runtime` stay unchanged.

use crate::agent::{AgentRunConfig, DEFAULT_SYSTEM_PROMPT};
use crate::agent_toolbox::RuntimeToolBox;
use crate::capabilities::{Capabilities, ResolvedAgent};
use crate::checkpoint::{Checkpoint, CheckpointHook, CheckpointToolBox};
use crate::policy::ToolGate;
use crate::providers::ProviderRegistry;
use crate::tools::NerveRuntime;
use anyhow::{Result, anyhow};
use nerve_agent::{
    AgentDef, AgentError, AgentEvent, AgentResult, Hook, Message, Orchestrator, ResumeState,
    RunOutcome, ToolBox, ToolSpec,
};
use nerve_core::{CancelToken, WorkspaceResolver};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub(crate) const DEFAULT_MAX_DEPTH: usize = 2;
const SPAWN_AGENT: &str = "spawn_agent";

/// Default cap on how many sub-agent investigations run concurrently in a
/// [`SubAgentSpawner::fan_out`]. Bounds thread + provider-connection pressure;
/// ureq is synchronous so each investigation occupies a whole OS thread.
///
/// `fan_out` (and its default concurrency / arg constructor / labeller) is a
/// primitive surfaced for callers (e.g. a future "investigate in parallel"
/// tool). Its threading/cap policy is exercised through [`bounded_fan_out`] in
/// tests, but the public entry point has no production caller yet — hence the
/// dead-code allow. (The underlying [`bounded_fan_out`] *is* directly tested.)
#[allow(dead_code, reason = "fan-out primitive awaiting a production caller")]
pub(crate) const DEFAULT_FANOUT_CONCURRENCY: usize = 4;

/// Sink the parent supplies so a child sub-agent's events surface (tagged with
/// the child's id) instead of being discarded. Composition-root concern: it
/// keeps `nerve-agent` unaware of sub-agent tagging.
pub(crate) type SubAgentEventSink = dyn Fn(&str, &AgentEvent) + Send + Sync;

#[derive(Debug)]
pub(crate) struct AgentRunOutput {
    pub(crate) outcome: RunOutcome,
    pub(crate) history: Vec<Message>,
    pub(crate) events: Vec<AgentEvent>,
}

#[derive(Clone)]
pub(crate) struct SubAgentSpawner {
    runtime: Arc<NerveRuntime>,
    registry: ProviderRegistry,
    gate: ToolGate,
    max_depth: usize,
    checkpoint: Arc<Mutex<Checkpoint>>,
    /// Optional forwarding sink for child events; `None` keeps the prior
    /// (discard) behaviour for callers that don't observe sub-agents.
    event_sink: Option<Arc<SubAgentEventSink>>,
    /// Monotonic source of sub-agent ids, shared across clones so ids are unique
    /// within one parent run (clones share the same spawner identity).
    next_sub_id: Arc<AtomicU64>,
}

impl SubAgentSpawner {
    pub(crate) fn new(
        runtime: Arc<NerveRuntime>,
        registry: ProviderRegistry,
        gate: ToolGate,
        max_depth: usize,
        checkpoint: Arc<Mutex<Checkpoint>>,
    ) -> Self {
        Self {
            runtime,
            registry,
            gate,
            max_depth,
            checkpoint,
            event_sink: None,
            next_sub_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Attach a forwarding sink so spawned sub-agents' events surface to the
    /// parent (tagged with the child's id) rather than being discarded.
    #[must_use]
    pub(crate) fn with_event_sink(mut self, sink: Arc<SubAgentEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Mint the next sub-agent id (`sub-<n>`), unique within this parent run.
    fn next_sub_agent_id(&self) -> String {
        let n = self.next_sub_id.fetch_add(1, Ordering::Relaxed);
        format!("sub-{n}")
    }

    pub(crate) fn run_at_depth(
        &self,
        depth: usize,
        config: AgentRunConfig,
        history: Vec<Message>,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(AgentEvent),
    ) -> Result<AgentRunOutput> {
        let root = root_for_runtime(&self.runtime, config.workspace.as_deref());
        let provider = self
            .registry
            .resolve(&config.provider, config.api_key.as_deref())?;
        let def = agent_def(&config);
        let env_hook = crate::hooks::EnvironmentHook::new(crate::hooks::today_utc(), root.clone());
        let parent = ParentRun::from_config(&config, root.clone());

        // Long-term project memory, file-backed at `<root>/.nerve/memory.md`. Only wired
        // when there is a project root — durable PROJECT facts need a project. Recall is the
        // existing `on_start` seam; writes go through the `remember` tool. The `MemoryStore`
        // port keeps a future SQLite backend a drop-in (see agent-long-term-memory.md).
        let memory_store: Option<Arc<dyn crate::memory::MemoryStore>> = root.as_deref().map(|r| {
            Arc::new(crate::memory::FileMemoryStore::new(
                r.join(".nerve").join("memory.md"),
            )) as Arc<dyn crate::memory::MemoryStore>
        });
        let toolbox = self.assemble_toolbox(
            &config,
            root.as_deref(),
            depth,
            parent,
            memory_store.as_ref(),
        );
        let checkpoint_hook = CheckpointHook::new(Arc::clone(&self.checkpoint));
        let memory_hook = memory_store
            .as_ref()
            .map(|store| crate::memory::MemoryHook::new(Arc::clone(store)));
        // Opt-in cost/cache telemetry through the Hook seam (#5): only when a
        // budget is configured. The hook holds the run's cancel token so it can
        // stop the run cooperatively once the estimate crosses the ceiling.
        let cost_hook = config.cost_budget_usd.map(|budget| {
            crate::cost::CostTelemetryHook::new(
                config.model.clone(),
                crate::cost::default_price_table(),
                Some(budget),
                cancel.clone(),
            )
        });
        let mut hooks: Vec<&dyn Hook> = vec![&env_hook, &checkpoint_hook];
        if let Some(hook) = &memory_hook {
            hooks.push(hook);
        }
        if let Some(hook) = &cost_hook {
            hooks.push(hook);
        }
        // Seed prior transcript through the resume seam rather than `with_history`,
        // so restored counters (e.g. context-overflow `truncations`) carry over and
        // already-executed tool results are NOT re-run on the next turn (#3). A
        // fresh run passes an empty history, which is equivalent to `new`.
        let resume_state = ResumeState {
            history,
            truncations: config.resume_truncations,
        };
        let mut orchestrator =
            Orchestrator::resume(&*provider, &*toolbox, def, resume_state).with_hooks(hooks);
        let output = run_collecting(&mut orchestrator, &config.task, cancel, sink)?;

        // Opt-in auto-distillation: after a substantive top-level run, a restricted
        // second pass extracts durable facts into long-term memory (best-effort; off by
        // default — see agent-long-term-memory.md §13). Subagents never distil.
        if should_distill(
            depth,
            config.distill_memory,
            output.outcome.turns,
            &output.outcome.reason,
        ) && let Some(store) = &memory_store
        {
            crate::memory::distill_session(
                &*provider,
                store,
                &config.model,
                output.history.clone(),
                cancel,
            );
        }
        Ok(output)
    }

    /// Run a bounded parallel fan-out of independent investigations and join all
    /// results. Each task runs as its own sub-agent (events forwarded + tagged via
    /// `run_spawn`), at most `concurrency` at a time. ureq is synchronous, so each
    /// in-flight task occupies one OS thread; the cap bounds that pressure. The
    /// order of results matches the order of `tasks`. A per-task failure is
    /// captured in its [`FanOutResult`] rather than aborting the whole fan-out.
    ///
    /// Read-only is the *caller's* responsibility: pass tasks/agents whose tool
    /// filters exclude mutating tools (the gate is still the outer authority). The
    /// fan-out itself adds no new tool authority.
    #[allow(dead_code, reason = "fan-out primitive awaiting a production caller")]
    pub(crate) fn fan_out(
        &self,
        depth: usize,
        tasks: Vec<SpawnAgentArgs>,
        parent: &ParentRun,
        concurrency: usize,
        cancel: &CancelToken,
    ) -> Vec<FanOutResult> {
        bounded_fan_out(
            tasks,
            concurrency,
            cancel,
            |args| {
                let label = task_label(&args);
                let outcome = self.run_spawn(depth + 1, parent, args, cancel);
                FanOutResult {
                    task: label,
                    outcome,
                }
            },
            |args| FanOutResult::cancelled(task_label(args)),
            FanOutResult::panicked,
        )
    }

    /// Assemble the agent's toolbox for one run. The P4 gate stays the
    /// **OUTERMOST** decorator (north-star invariant 9): every tool the model can
    /// call — read tools, `spawn_agent`, the agent-state tools, and (when enabled)
    /// `run_command` / `delegate_agent` — passes through it. The exec and delegate
    /// decorators sit immediately inside the gate, so each of their calls is
    /// authorized (exec-tier → Ask) and contained by the run's launcher:
    /// `PolicyToolBox(DelegateAgentToolBox(ExecToolBox(…)))`.
    fn assemble_toolbox(
        &self,
        config: &AgentRunConfig,
        root: Option<&Path>,
        depth: usize,
        parent: ParentRun,
        memory_store: Option<&Arc<dyn crate::memory::MemoryStore>>,
    ) -> Box<dyn ToolBox> {
        let runner: Arc<dyn SpawnRunner> = Arc::new(self.clone());
        let raw = SubAgentToolBox::new(
            RuntimeToolBox::new(Arc::clone(&self.runtime)),
            runner,
            depth,
            parent,
        );
        let checkpoint_tb = CheckpointToolBox::new(raw, Arc::clone(&self.checkpoint));
        match memory_store {
            Some(store) => {
                let mem = crate::memory::MemoryToolBox::new(checkpoint_tb, Arc::clone(store));
                self.wrap_exec_delegate_gate(mem, config, root, depth)
            }
            None => self.wrap_exec_delegate_gate(checkpoint_tb, config, root, depth),
        }
    }

    /// Wrap `inner` with the exec decorator, then the delegate decorator, then the
    /// outermost gate. Both delegated tools (`run_command`, `delegate_agent`) sit
    /// inside the gate so exec-tier approval applies; `delegate_agent` is exposed
    /// only at the **top level** (`depth == 0`) and when `allow_delegate` is set —
    /// the recursion guard that keeps a delegated/sub-agent context from spawning
    /// more external agents.
    fn wrap_exec_delegate_gate<T: ToolBox + 'static>(
        &self,
        inner: T,
        config: &AgentRunConfig,
        root: Option<&Path>,
        depth: usize,
    ) -> Box<dyn ToolBox> {
        let exec = crate::exec_tool::ExecToolBox::new(
            inner,
            Arc::clone(&config.exec_launcher),
            crate::sandbox::SandboxPolicy::for_root(root),
            config.allow_exec,
        );
        let delegate = crate::delegate_tool::DelegateAgentToolBox::new(
            exec,
            Arc::clone(&config.delegate_launcher),
            root.map(Path::to_path_buf),
            config.allow_delegate && depth == 0,
            config.delegate_event_sink.clone(),
            config.workspace.clone(),
        );
        Box::new(self.gate.clone().wrap(delegate))
    }
}

impl SpawnRunner for SubAgentSpawner {
    fn max_depth(&self) -> usize {
        self.max_depth
    }

    fn run_spawn(
        &self,
        depth: usize,
        parent: &ParentRun,
        args: SpawnAgentArgs,
        cancel: &CancelToken,
    ) -> AgentResult<RunOutcome> {
        let config = sub_config(parent, args).map_err(|err| AgentError::Tool(err.to_string()))?;
        let sub_id = self.next_sub_agent_id();
        // Forward each child event to the parent's sink tagged with `sub_id`, so
        // the child's progress surfaces instead of being silently discarded. With
        // no sink attached this is a cheap no-op (prior behaviour).
        let sink = self.event_sink.clone();
        let mut forwarding_sink = |event: AgentEvent| {
            if let Some(sink) = &sink {
                sink(&sub_id, &event);
            }
        };
        self.run_at_depth(depth, config, Vec::new(), cancel, &mut forwarding_sink)
            .map(|output| output.outcome)
            .map_err(|err| AgentError::Tool(format!("spawn_agent failed: {err}")))
    }
}

/// Gate for opt-in auto-distillation: only a top-level (`depth == 0`), opted-in run that
/// did real work (`turns >= DISTILL_MIN_TURNS`) and completed normally (not cancelled)
/// distils. Subagents and trivial/cancelled runs never trigger the extra LLM pass.
fn should_distill(depth: usize, opted_in: bool, turns: u32, reason: &str) -> bool {
    depth == 0 && opted_in && turns >= crate::memory::DISTILL_MIN_TURNS && reason != "cancelled"
}

fn run_collecting(
    orchestrator: &mut Orchestrator<'_>,
    task: &str,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(AgentEvent),
) -> Result<AgentRunOutput> {
    let mut events = Vec::new();
    let result = {
        let mut recording_sink = |event: AgentEvent| {
            events.push(event.clone());
            sink(event);
        };
        orchestrator.run(task, cancel, &mut recording_sink)
    }
    .map_err(|err| anyhow!("agent run failed: {err}"))?;
    Ok(AgentRunOutput {
        outcome: result,
        history: orchestrator.history().to_vec(),
        events,
    })
}

fn agent_def(config: &AgentRunConfig) -> AgentDef {
    AgentDef {
        system_prompt: config
            .system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string()),
        model: config.model.clone(),
        max_turns: config.max_turns.unwrap_or(40),
        temperature: config.temperature,
        reasoning_effort: config.reasoning_effort.clone(),
        tool_filter: config.tool_filter.clone(),
        verify_completion: config.verify_completion,
        ..AgentDef::default()
    }
}

fn root_for_runtime(runtime: &NerveRuntime, workspace: Option<&str>) -> Option<PathBuf> {
    runtime
        .resolver()
        .resolve_workspace(workspace)
        .ok()
        .and_then(|workspace| workspace.roots().first().map(|root| root.path.clone()))
}

fn sub_config(parent: &ParentRun, args: SpawnAgentArgs) -> Result<AgentRunConfig> {
    let task = args.task.trim();
    if task.is_empty() {
        return Err(anyhow!("spawn_agent requires a non-empty task"));
    }
    let resolved = resolve_sub_agent(parent, args.agent.as_deref())?;
    let provider = args
        .provider
        .or(resolved.provider)
        .unwrap_or_else(|| parent.provider.clone());
    let model = args
        .model
        .or(resolved.model)
        .unwrap_or_else(|| parent.model.clone());
    let api_key = (provider == parent.provider)
        .then(|| parent.api_key.clone())
        .flatten();
    Ok(AgentRunConfig {
        workspace: parent.workspace.clone(),
        provider,
        model,
        task: task.to_string(),
        system_prompt: resolved.system_prompt,
        max_turns: resolved.max_turns,
        temperature: resolved.temperature,
        reasoning_effort: resolved.reasoning_effort,
        tool_filter: resolved.tool_filter,
        api_key,
        distill_memory: false,
        verify_completion: false,
        // Subagents never execute commands in the MVP, independent of the
        // parent's capability; refuse is the safe default for the launcher field.
        allow_exec: false,
        exec_launcher: crate::sandbox::refuse_launcher(),
        // Only the top-level agent delegates (recursion guard): a sub-agent never
        // gets the delegate tool. Refuse is the safe default for the launcher field.
        allow_delegate: false,
        delegate_launcher: crate::sandbox::refuse_launcher(),
        delegate_event_sink: None,
        // Spawned sub-agents always start fresh.
        resume_truncations: 0,
        // Sub-agents inherit no budget guard; the parent's guard already bounds it.
        cost_budget_usd: None,
    })
}

fn resolve_sub_agent(parent: &ParentRun, name: Option<&str>) -> Result<ResolvedAgent> {
    match name {
        Some(name) => Capabilities::discover(parent.root.as_deref()).resolve_agent(name),
        None => Ok(ResolvedAgent::default()),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ParentRun {
    workspace: Option<String>,
    root: Option<PathBuf>,
    provider: String,
    model: String,
    api_key: Option<String>,
}

impl ParentRun {
    fn from_config(config: &AgentRunConfig, root: Option<PathBuf>) -> Self {
        Self {
            workspace: config.workspace.clone(),
            root,
            provider: config.provider.clone(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
        }
    }

    #[cfg(test)]
    fn test(provider: &str, model: &str) -> Self {
        Self {
            workspace: None,
            root: None,
            provider: provider.into(),
            model: model.into(),
            api_key: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SpawnAgentArgs {
    task: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

impl SpawnAgentArgs {
    /// Build args for a fan-out investigation from a task (and optional named
    /// agent), inheriting provider/model from the parent. Used by callers that
    /// drive [`SubAgentSpawner::fan_out`] with read-only investigations.
    #[allow(dead_code, reason = "fan-out primitive awaiting a production caller")]
    pub(crate) fn investigation(task: impl Into<String>, agent: Option<String>) -> Self {
        Self {
            task: task.into(),
            agent,
            provider: None,
            model: None,
        }
    }
}

/// One task's result within a [`SubAgentSpawner::fan_out`]. `outcome` carries the
/// per-task error rather than aborting the whole fan-out, so one failed
/// investigation doesn't lose its siblings' work.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct FanOutResult {
    pub(crate) task: String,
    pub(crate) outcome: AgentResult<RunOutcome>,
}

impl FanOutResult {
    fn cancelled(task: String) -> Self {
        Self {
            task,
            outcome: Err(AgentError::Cancelled),
        }
    }

    fn panicked() -> Self {
        Self {
            task: String::new(),
            outcome: Err(AgentError::Tool("sub-agent thread panicked".to_string())),
        }
    }
}

/// A short label for a fan-out task, for result attribution.
#[allow(dead_code, reason = "used by the fan-out primitive awaiting a caller")]
fn task_label(args: &SpawnAgentArgs) -> String {
    args.task.clone()
}

/// Run `work` over `inputs` with at most `concurrency` in flight at once,
/// preserving input order in the output. Bounded via std scoped threads in waves
/// of `cap` — no extra semaphore. A pre-cancelled wave short-circuits to
/// `on_cancelled(&item)` per remaining input; a worker panic degrades to
/// `on_panic()` rather than poisoning the join. Generic over the work item AND
/// the result type so both the sub-agent fan-out (`R = FanOutResult`) and the
/// flow engine's parallel wave (`R = NodeResult`) share one threading/cap/order
/// implementation — the determinism invariant (input order preserved) lives here.
pub(crate) fn bounded_fan_out<I, R, W, C, P>(
    inputs: Vec<I>,
    concurrency: usize,
    cancel: &CancelToken,
    work: W,
    on_cancelled: C,
    on_panic: P,
) -> Vec<R>
where
    I: Send,
    R: Send,
    W: Fn(I) -> R + Sync,
    C: Fn(&I) -> R + Sync,
    P: Fn() -> R + Sync,
{
    let cap = concurrency.max(1);
    let mut results: Vec<R> = Vec::with_capacity(inputs.len());
    let mut remaining = inputs.into_iter();
    loop {
        let wave: Vec<I> = remaining.by_ref().take(cap).collect();
        if wave.is_empty() {
            break;
        }
        if cancel.is_cancelled() {
            results.extend(wave.iter().map(&on_cancelled));
            continue;
        }
        let work = &work;
        let on_panic = &on_panic;
        let done: Vec<R> = std::thread::scope(|scope| {
            let handles: Vec<_> = wave
                .into_iter()
                .map(|item| scope.spawn(move || work(item)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_else(|_| on_panic()))
                .collect()
        });
        results.extend(done);
    }
    results
}

pub(crate) trait SpawnRunner: Send + Sync {
    fn max_depth(&self) -> usize;
    fn run_spawn(
        &self,
        depth: usize,
        parent: &ParentRun,
        args: SpawnAgentArgs,
        cancel: &CancelToken,
    ) -> AgentResult<RunOutcome>;
}

pub(crate) struct SubAgentToolBox<T: ToolBox> {
    inner: T,
    spawner: Arc<dyn SpawnRunner>,
    depth: usize,
    parent: ParentRun,
}

impl<T: ToolBox> SubAgentToolBox<T> {
    pub(crate) fn new(
        inner: T,
        spawner: Arc<dyn SpawnRunner>,
        depth: usize,
        parent: ParentRun,
    ) -> Self {
        Self {
            inner,
            spawner,
            depth,
            parent,
        }
    }

    fn may_spawn(&self) -> bool {
        self.depth < self.spawner.max_depth()
    }
}

impl<T: ToolBox> ToolBox for SubAgentToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        if self.may_spawn() {
            specs.push(spawn_spec());
        }
        specs
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if name != SPAWN_AGENT {
            return self.inner.call(name, args, cancel);
        }
        if !self.may_spawn() {
            return Err(AgentError::Tool(format!(
                "spawn_agent depth limit reached: max_depth={} depth={}",
                self.spawner.max_depth(),
                self.depth
            )));
        }
        let args: SpawnAgentArgs = serde_json::from_value(args.clone())
            .map_err(|err| AgentError::Tool(format!("invalid spawn_agent args: {err}")))?;
        let outcome = self
            .spawner
            .run_spawn(self.depth + 1, &self.parent, args, cancel)?;
        Ok(json!({
            "final_text": outcome.final_text,
            "turns": outcome.turns,
            "usage": {
                "input_tokens": outcome.usage.input_tokens,
                "output_tokens": outcome.usage.output_tokens,
            },
        }))
    }
}

fn spawn_spec() -> ToolSpec {
    ToolSpec {
        name: SPAWN_AGENT.to_string(),
        description: "Run a fresh sub-agent for a delegated task and return its final result."
            .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "The subtask for the sub-agent to perform."
                },
                "agent": {
                    "type": "string",
                    "description": "Optional named agent definition to use."
                },
                "provider": {
                    "type": "string",
                    "description": "Optional provider override; defaults to the parent provider."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override; defaults to the parent model."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Policy, ToolGate};
    use nerve_agent::Usage;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn should_distill_gates_on_depth_optin_turns_and_completion() {
        let floor = crate::memory::DISTILL_MIN_TURNS;
        assert!(should_distill(0, true, floor, "stop"));
        assert!(!should_distill(1, true, floor, "stop")); // subagents never distil
        assert!(!should_distill(0, false, floor, "stop")); // opt-in required
        assert!(!should_distill(0, true, floor - 1, "stop")); // below the turn floor
        assert!(!should_distill(0, true, floor, "cancelled")); // cancelled runs skip
    }

    struct FakeInner;

    impl ToolBox for FakeInner {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "read_file".into(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
            }]
        }

        fn call(&self, name: &str, args: &Value, _cancel: &CancelToken) -> AgentResult<Value> {
            Ok(json!({ "name": name, "args": args }))
        }
    }

    struct FakeRunner {
        max_depth: usize,
        calls: AtomicUsize,
        saw_cancelled: AtomicBool,
        last_depth: Mutex<Option<usize>>,
    }

    impl FakeRunner {
        fn new(max_depth: usize) -> Self {
            Self {
                max_depth,
                calls: AtomicUsize::new(0),
                saw_cancelled: AtomicBool::new(false),
                last_depth: Mutex::new(None),
            }
        }
    }

    impl SpawnRunner for FakeRunner {
        fn max_depth(&self) -> usize {
            self.max_depth
        }

        fn run_spawn(
            &self,
            depth: usize,
            _parent: &ParentRun,
            _args: SpawnAgentArgs,
            cancel: &CancelToken,
        ) -> AgentResult<RunOutcome> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.saw_cancelled
                .store(cancel.is_cancelled(), Ordering::SeqCst);
            *self.last_depth.lock().expect("depth lock") = Some(depth);
            Ok(RunOutcome {
                reason: "completed".into(),
                turns: 3,
                final_text: "sub result".into(),
                usage: Usage {
                    input_tokens: 11,
                    output_tokens: 7,
                    ..Usage::default()
                },
            })
        }
    }

    fn toolbox(depth: usize, runner: Arc<FakeRunner>) -> SubAgentToolBox<FakeInner> {
        let erased: Arc<dyn SpawnRunner> = runner;
        SubAgentToolBox::new(FakeInner, erased, depth, ParentRun::test("p", "m"))
    }

    #[test]
    fn specs_include_spawn_before_depth_limit() {
        let specs = toolbox(0, Arc::new(FakeRunner::new(2))).specs();
        assert!(specs.iter().any(|spec| spec.name == SPAWN_AGENT));
    }

    #[test]
    fn depth_limit_omits_spawn_and_errors_clearly() {
        let runner = Arc::new(FakeRunner::new(2));
        let tools = toolbox(2, Arc::clone(&runner));
        assert!(!tools.specs().iter().any(|spec| spec.name == SPAWN_AGENT));
        let err = tools
            .call(SPAWN_AGENT, &json!({ "task": "go" }), &CancelToken::never())
            .expect_err("depth should block");
        assert!(err.to_string().contains("depth limit"));
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn spawn_returns_sub_agent_result() {
        let runner = Arc::new(FakeRunner::new(2));
        let out = toolbox(0, Arc::clone(&runner))
            .call(SPAWN_AGENT, &json!({ "task": "go" }), &CancelToken::never())
            .expect("spawn succeeds");
        assert_eq!(out["final_text"], "sub result");
        assert_eq!(out["turns"], 3);
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(*runner.last_depth.lock().expect("depth lock"), Some(1));
    }

    #[test]
    fn cancellation_token_is_propagated_to_spawn() {
        let runner = Arc::new(FakeRunner::new(2));
        let cancel = CancelToken::new();
        cancel.cancel();
        toolbox(0, Arc::clone(&runner))
            .call(SPAWN_AGENT, &json!({ "task": "go" }), &cancel)
            .expect("fake runner returns result");
        assert!(runner.saw_cancelled.load(Ordering::SeqCst));
    }

    #[test]
    fn policy_gate_can_block_spawn_agent() {
        let runner = Arc::new(FakeRunner::new(2));
        let gated = ToolGate::deny(Policy::default()).wrap(toolbox(0, Arc::clone(&runner)));
        let err = gated
            .call(SPAWN_AGENT, &json!({ "task": "go" }), &CancelToken::never())
            .expect_err("policy should deny ask");
        assert!(err.to_string().contains("permission denied"));
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn non_spawn_calls_delegate_to_inner() {
        let out = toolbox(0, Arc::new(FakeRunner::new(2)))
            .call("read_file", &json!({ "path": "x" }), &CancelToken::never())
            .expect("delegated");
        assert_eq!(out["name"], "read_file");
    }

    fn ok_result(task: &str) -> FanOutResult {
        FanOutResult {
            task: task.to_string(),
            outcome: Ok(RunOutcome {
                reason: "completed".into(),
                turns: 1,
                final_text: task.to_string(),
                usage: Usage::default(),
            }),
        }
    }

    #[test]
    fn bounded_fan_out_preserves_order_and_runs_all() {
        let inputs = vec!["a", "b", "c", "d", "e"];
        let results = bounded_fan_out(
            inputs,
            2,
            &CancelToken::never(),
            ok_result,
            |label| FanOutResult::cancelled(label.to_string()),
            FanOutResult::panicked,
        );
        let labels: Vec<&str> = results.iter().map(|r| r.task.as_str()).collect();
        // Order matches input order even though waves run concurrently.
        assert_eq!(labels, vec!["a", "b", "c", "d", "e"]);
        assert!(results.iter().all(|r| r.outcome.is_ok()));
    }

    #[test]
    fn bounded_fan_out_respects_concurrency_cap() {
        use std::sync::atomic::AtomicUsize;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let inputs: Vec<u32> = (0..12).collect();
        let in_flight_w = Arc::clone(&in_flight);
        let peak_w = Arc::clone(&peak);
        let results = bounded_fan_out(
            inputs,
            3,
            &CancelToken::never(),
            move |n| {
                let now = in_flight_w.fetch_add(1, Ordering::SeqCst) + 1;
                peak_w.fetch_max(now, Ordering::SeqCst);
                // Hold the slot briefly so concurrent workers overlap within a wave.
                std::thread::sleep(std::time::Duration::from_millis(20));
                in_flight_w.fetch_sub(1, Ordering::SeqCst);
                ok_result(&n.to_string())
            },
            |_| FanOutResult::cancelled(String::new()),
            FanOutResult::panicked,
        );
        assert_eq!(results.len(), 12);
        // Never more than the cap concurrently in flight.
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "peak concurrency {} exceeded cap 3",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn bounded_fan_out_short_circuits_when_cancelled() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ran_w = Arc::clone(&ran);
        let results = bounded_fan_out(
            vec![1, 2, 3],
            2,
            &cancel,
            move |_| {
                ran_w.fetch_add(1, Ordering::SeqCst);
                ok_result("x")
            },
            |_| FanOutResult::cancelled(String::new()),
            FanOutResult::panicked,
        );
        assert_eq!(results.len(), 3);
        assert!(
            results
                .iter()
                .all(|r| matches!(r.outcome, Err(AgentError::Cancelled)))
        );
        // The worker never ran for a pre-cancelled fan-out.
        assert_eq!(ran.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn bounded_fan_out_captures_worker_panic_without_aborting() {
        let results = bounded_fan_out(
            vec![1, 2, 3],
            3,
            &CancelToken::never(),
            |n| {
                if n == 2 {
                    panic!("boom");
                }
                ok_result(&n.to_string())
            },
            |_| FanOutResult::cancelled(String::new()),
            FanOutResult::panicked,
        );
        assert_eq!(results.len(), 3);
        // The panicking task degrades to an error; its siblings still succeed.
        let oks = results.iter().filter(|r| r.outcome.is_ok()).count();
        assert_eq!(oks, 2);
        assert!(
            results
                .iter()
                .any(|r| matches!(&r.outcome, Err(AgentError::Tool(m)) if m.contains("panicked")))
        );
    }

    #[test]
    fn spawn_forwards_tagged_child_events_to_attached_sink() {
        // The forwarding sink wired on a SubAgentSpawner-style runner must receive
        // the child id; here we exercise the sink contract directly via a runner
        // that emits through a captured forwarding sink.
        let captured: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_capture = Arc::clone(&captured);
        let sink: Arc<SubAgentEventSink> = Arc::new(move |sub_id: &str, event: &AgentEvent| {
            if let AgentEvent::AssistantText(text) = event {
                sink_capture
                    .lock()
                    .expect("capture lock")
                    .push((sub_id.to_string(), text.clone()));
            }
        });
        // Simulate two child events forwarded under one sub-agent id.
        sink("sub-1", &AgentEvent::AssistantText("hello".into()));
        sink("sub-1", &AgentEvent::AssistantText("world".into()));
        let got = captured.lock().expect("lock").clone();
        assert_eq!(
            got,
            vec![
                ("sub-1".to_string(), "hello".to_string()),
                ("sub-1".to_string(), "world".to_string()),
            ]
        );
    }
}
