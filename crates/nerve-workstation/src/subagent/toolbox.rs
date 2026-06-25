//! The `spawn_agent` ToolBox seam: from the orchestrator's perspective spawning a
//! sub-agent is just another tool. [`SubAgentToolBox`] wraps an inner [`ToolBox`],
//! advertising `spawn_agent` only while under the depth cap, and dispatches a spawn
//! call through the [`SpawnRunner`] port (implemented by the parent module's
//! `SubAgentSpawner`). Kept separate from the run-assembly logic so each file stays a
//! single responsibility.

use super::{ParentRun, SpawnAgentArgs};
use nerve_agent::{AgentError, AgentResult, RunOutcome, ToolBox, ToolSpec};
use nerve_core::CancelToken;
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) const SPAWN_AGENT: &str = "spawn_agent";

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
