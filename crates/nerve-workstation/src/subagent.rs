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
    AgentDef, AgentError, AgentEvent, AgentResult, Hook, Message, Orchestrator, RunOutcome,
    ToolBox, ToolSpec,
};
use nerve_core::{CancelToken, WorkspaceResolver};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub(crate) const DEFAULT_MAX_DEPTH: usize = 2;
const SPAWN_AGENT: &str = "spawn_agent";

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
        }
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
        let runner: Arc<dyn SpawnRunner> = Arc::new(self.clone());
        let parent = ParentRun::from_config(&config, root.clone());
        let raw = SubAgentToolBox::new(
            RuntimeToolBox::new(Arc::clone(&self.runtime)),
            runner,
            depth,
            parent,
        );
        let gated = self.gate.clone().wrap(raw);
        let checkpoint_tb = CheckpointToolBox::new(gated, Arc::clone(&self.checkpoint));

        // Long-term project memory, file-backed at `<root>/.nerve/memory.md`. Only wired
        // when there is a project root — durable PROJECT facts need a project. Recall is the
        // existing `on_start` seam; writes go through the `remember` tool. The `MemoryStore`
        // port keeps a future SQLite backend a drop-in (see agent-long-term-memory.md).
        let memory_store: Option<Arc<dyn crate::memory::MemoryStore>> = root.as_deref().map(|r| {
            Arc::new(crate::memory::FileMemoryStore::new(
                r.join(".nerve").join("memory.md"),
            )) as Arc<dyn crate::memory::MemoryStore>
        });
        let toolbox: Box<dyn ToolBox> = match &memory_store {
            Some(store) => Box::new(crate::memory::MemoryToolBox::new(
                checkpoint_tb,
                Arc::clone(store),
            )),
            None => Box::new(checkpoint_tb),
        };
        let checkpoint_hook = CheckpointHook::new(Arc::clone(&self.checkpoint));
        let memory_hook = memory_store
            .as_ref()
            .map(|store| crate::memory::MemoryHook::new(Arc::clone(store)));
        let mut hooks: Vec<&dyn Hook> = vec![&env_hook, &checkpoint_hook];
        if let Some(hook) = &memory_hook {
            hooks.push(hook);
        }
        let mut orchestrator = Orchestrator::new(&*provider, &*toolbox, def)
            .with_history(history)
            .with_hooks(hooks);
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
        let mut internal_sink = |_event: AgentEvent| {};
        self.run_at_depth(depth, config, Vec::new(), cancel, &mut internal_sink)
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

#[derive(Debug, Deserialize)]
pub(crate) struct SpawnAgentArgs {
    task: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
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
}
