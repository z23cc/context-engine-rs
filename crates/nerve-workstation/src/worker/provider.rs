//! [`ProviderWorker`] — the [`AgentWorker`] over the in-process provider substrate.
//!
//! This wraps the SHIPPED [`SubAgentSpawner::run_at_depth`](crate::subagent)
//! without changing it: `start` builds an [`AgentRunConfig`], runs the orchestrator
//! at depth 0 under the SAME outermost `PolicyToolBox` gate that `run_at_depth`
//! assembles (`PolicyToolBox(DelegateAgentToolBox(ExecToolBox(RuntimeToolBox)))`),
//! and maps the existing [`AgentEvent`] stream 1:1 onto [`WorkerEvent::Step`] via
//! [`agent_event_kind`](crate::agent_event::agent_event_kind) — the same mapper the
//! session manager and `agent.run` use. `steer` runs another orchestrator turn over
//! the retained transcript through the existing `ResumeState`/`with_history` seam.
//!
//! This is the in-process worker with the FULL Nerve tool surface
//! (search/read/codemap/navigate/edit/semantic) on the shared snapshot.
#![allow(
    dead_code,
    reason = "C0 worker port awaits its C1 engine caller (mirrors subagent::bounded_fan_out)"
)]

use super::{
    AgentWorker, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerKind, WorkerSession,
    WorkerTask,
};
use crate::agent::AgentRunConfig;
use crate::checkpoint::Checkpoint;
use crate::policy::ToolGate;
use crate::providers::ProviderRegistry;
use crate::subagent::SubAgentSpawner;
use crate::tools::NerveRuntime;
use nerve_agent::{AgentEvent, Message};
use nerve_core::CancelToken;
use std::sync::{Arc, Mutex};

/// A worker backed by an in-process [`LlmProvider`](nerve_agent::provider::LlmProvider)
/// loop. Holds the shared deps a [`SubAgentSpawner`] needs (runtime / registry /
/// gate / max-depth) plus the provider/model identity; `start` drives turn 1.
#[derive(Clone)]
pub(crate) struct ProviderWorker {
    runtime: Arc<NerveRuntime>,
    registry: ProviderRegistry,
    gate: ToolGate,
    max_depth: usize,
    provider: String,
    model: String,
}

impl ProviderWorker {
    /// Build a provider worker sharing the runtime / registry / gate, targeting
    /// `provider`/`model`.
    pub(crate) fn new(
        runtime: Arc<NerveRuntime>,
        registry: ProviderRegistry,
        gate: ToolGate,
        max_depth: usize,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            runtime,
            registry,
            gate,
            max_depth,
            provider: provider.into(),
            model: model.into(),
        }
    }

    /// Assemble the [`AgentRunConfig`] for one task. Refuses exec/delegate by
    /// default (a provider worker reaches tools through the gated [`RuntimeToolBox`],
    /// not the exec/delegate launchers); the model override and tool filter flow
    /// from the [`WorkerTask`].
    fn run_config(&self, task: &WorkerTask) -> AgentRunConfig {
        AgentRunConfig {
            workspace: None,
            provider: self.provider.clone(),
            model: task.model.clone().unwrap_or_else(|| self.model.clone()),
            task: task.prompt.clone(),
            system_prompt: None,
            max_turns: None,
            temperature: None,
            reasoning_effort: None,
            tool_filter: task.tool_filter.clone(),
            api_key: None,
            distill_memory: false,
            verify_completion: false,
            // A provider worker reaches tools only through the gated RuntimeToolBox;
            // exec + delegate stay refused (the same posture a session turn uses).
            allow_exec: false,
            exec_launcher: crate::sandbox::refuse_launcher(),
            allow_delegate: false,
            delegate_launcher: crate::sandbox::refuse_launcher(),
            delegate_event_sink: None,
            resume_truncations: 0,
            // The task's per-node `BudgetGrant` (carved from the fleet envelope in
            // `node_grant`, intersected so it can only narrow) is installed as this
            // turn's in-turn cost ceiling: `run_at_depth` arms a `CostTelemetryHook`
            // with it, so the node cooperatively cancels the moment its OWN running
            // estimate crosses the grant — not only at the whole-flow `BudgetLedger`
            // fold between turns (finding I).
            cost_budget_usd: grant_cost_ceiling(&task.budget),
        }
    }

    /// Build the [`SubAgentSpawner`] over the shared deps — exactly what
    /// `run_agent`/`run_at_depth` construct, so the toolbox assembly + gate are
    /// identical.
    fn spawner(&self) -> SubAgentSpawner {
        SubAgentSpawner::new(
            Arc::clone(&self.runtime),
            self.registry.clone(),
            self.gate.clone(),
            self.max_depth,
            Arc::new(Mutex::new(Checkpoint::new())),
        )
    }
}

impl AgentWorker for ProviderWorker {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Provider {
            provider: self.provider.clone(),
            model: self.model.clone(),
        }
    }

    fn capability(&self) -> nerve_runtime::RiskTier {
        // A provider worker can reach the edit tools on the shared snapshot, but
        // never exec/delegate (refused above). Worst-case is the edit tier.
        nerve_runtime::RiskTier::Edit
    }

    fn start(
        &self,
        task: &WorkerTask,
        _ctx: &WorkerContext,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        // `ctx` (root/snapshot/ledger/approver) is unused by a provider worker in
        // C0: it reaches tools through the gated RuntimeToolBox, raises approvals
        // through that gate (not `ctx.approver`), and does not pin a snapshot yet.
        // The engine (C1) threads the node-scoped context when it owns the run.
        let config = self.run_config(task);
        let output = run_provider_turn(&self.spawner(), config, Vec::new(), cancel, on_event)?;
        let result = outcome_to_result(&output.outcome);
        Ok(Box::new(ProviderSession {
            worker: self.clone(),
            history: output.history,
            last: result,
        }))
    }
}

/// A live provider session: retains the transcript so a steer resumes it through
/// the orchestrator's `ResumeState`/`with_history` seam (a fresh `Orchestrator`
/// turn over the prior history, never re-running already-executed tool calls).
struct ProviderSession {
    worker: ProviderWorker,
    history: Vec<Message>,
    last: TurnResult,
}

impl WorkerSession for ProviderSession {
    fn steer(
        &mut self,
        message: &str,
        cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        if cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        // A steer is a fresh orchestrator run over the retained history: build a
        // config whose task is the follow-up message, then seed the prior transcript
        // via `run_at_depth`'s history argument (the ResumeState seam).
        let config = self.worker.run_config(&steer_task(message));
        let history = std::mem::take(&mut self.history);
        let output = run_provider_turn(&self.worker.spawner(), config, history, cancel, on_event)?;
        self.history = output.history;
        let result = outcome_to_result(&output.outcome);
        self.last = result.clone();
        Ok(result)
    }

    fn interrupt(&self) {
        // The provider loop is driven synchronously inside `steer`; cooperative
        // cancellation flows through the `CancelToken` the caller supplies to
        // `steer`. There is no out-of-band child to signal here.
    }

    fn close(&mut self) {
        // No live subprocess to reap; dropping the retained history is enough.
        self.history.clear();
    }

    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

/// Drive one provider turn through the spawner, mapping each [`AgentEvent`] to a
/// [`WorkerEvent::Step`] (dropping events with no protocol projection). Returns the
/// run output (outcome + retained history) for the session to keep.
fn run_provider_turn(
    spawner: &SubAgentSpawner,
    config: AgentRunConfig,
    history: Vec<Message>,
    cancel: &CancelToken,
    on_event: &mut dyn FnMut(WorkerEvent),
) -> Result<crate::subagent::AgentRunOutput, WorkerError> {
    let mut sink = |event: AgentEvent| {
        if let Some(kind) = crate::agent_event::agent_event_kind(event) {
            on_event(WorkerEvent::Step(kind));
        }
    };
    spawner
        .run_at_depth(0, config, history, cancel, &mut sink)
        .map_err(|err| {
            if cancel.is_cancelled() {
                WorkerError::Cancelled
            } else {
                WorkerError::Turn(err.to_string())
            }
        })
}

/// Map a [`RunOutcome`](nerve_agent::RunOutcome) into the port's [`TurnResult`].
/// A graceful run is `ok`; an explicitly-cancelled run is not.
fn outcome_to_result(outcome: &nerve_agent::RunOutcome) -> TurnResult {
    TurnResult {
        ok: outcome.reason != "cancelled",
        text: outcome.final_text.clone(),
        usage: outcome.usage,
        cost_usd: None,
        timed_out: false,
    }
}

/// A throwaway task carrying just the steer message (the other fields are unused on
/// a steer — `run_config` only reads `prompt`/`model`/`tool_filter`/`budget`).
fn steer_task(message: &str) -> WorkerTask {
    WorkerTask {
        // A steer reuses the live session; its node id is supplied out of band by the
        // steer registry, so the throwaway task carries none.
        node_id: String::new(),
        prompt: message.to_string(),
        autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
        model: None,
        tool_filter: None,
        budget: super::BudgetGrant::default(),
    }
}

/// The in-turn USD cost ceiling a per-node [`BudgetGrant`](super::BudgetGrant) installs
/// on a provider run (finding I): the grant's `max_cost_usd`. `run_at_depth` arms a
/// [`CostTelemetryHook`](crate::cost::CostTelemetryHook) with it, so the node
/// cooperatively cancels its OWN turn once its running estimate crosses the carved
/// ceiling — the per-node brake, complementing the whole-flow `BudgetLedger` fold that
/// runs between turns. `None` (an unbudgeted flow's default grant) wires no ceiling, so
/// existing flows keep running unbounded as before.
fn grant_cost_ceiling(grant: &super::BudgetGrant) -> Option<f64> {
    grant.max_cost_usd
}

#[cfg(test)]
mod tests {
    use super::super::usage_step;
    use super::*;
    use nerve_runtime::AgentEventKind;

    #[test]
    fn outcome_to_result_marks_cancelled_runs_not_ok() {
        let cancelled = nerve_agent::RunOutcome {
            reason: "cancelled".into(),
            turns: 1,
            final_text: "partial".into(),
            usage: nerve_agent::Usage::default(),
        };
        assert!(!outcome_to_result(&cancelled).ok);

        let done = nerve_agent::RunOutcome {
            reason: "stop".into(),
            turns: 2,
            final_text: "done".into(),
            usage: nerve_agent::Usage {
                input_tokens: 9,
                output_tokens: 4,
                ..nerve_agent::Usage::default()
            },
        };
        let result = outcome_to_result(&done);
        assert!(result.ok);
        assert_eq!(result.text, "done");
        assert_eq!(result.usage.input_tokens, 9);
    }

    #[test]
    fn per_node_grant_arms_the_in_turn_cost_ceiling() {
        // Finding I: a provider node's carved per-node `BudgetGrant.max_cost_usd` must
        // flow onto the run as `cost_budget_usd`, so `run_at_depth` installs the
        // CostTelemetryHook that cancels the turn when its OWN estimate crosses the
        // grant. A capped grant yields the ceiling; a default (uncapped) grant yields
        // None (no in-turn brake), keeping existing flows unbounded.
        let grant = super::super::BudgetGrant {
            max_cost_usd: Some(0.25),
            max_tokens: Some(1000),
        };
        assert_eq!(
            grant_cost_ceiling(&grant),
            Some(0.25),
            "the carved per-node USD grant is the in-turn cost ceiling"
        );
        assert_eq!(
            grant_cost_ceiling(&super::super::BudgetGrant::default()),
            None,
            "a default (uncapped) grant wires no in-turn ceiling"
        );
    }

    #[test]
    fn run_config_threads_the_grant_onto_cost_budget_usd() {
        // The wiring is end-to-end: a WorkerTask carrying a per-node grant produces an
        // AgentRunConfig whose `cost_budget_usd` IS the grant ceiling (so the hook arms).
        let runtime = Arc::new(crate::tools::runtime(
            nerve_core::WorkspaceRegistry::default(),
        ));
        let worker = ProviderWorker::new(
            runtime,
            ProviderRegistry::default(),
            ToolGate::deny(crate::policy::Policy::default()),
            2,
            "xai",
            "grok",
        );
        let task = WorkerTask {
            node_id: "node-0".into(),
            prompt: "do it".into(),
            autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
            model: None,
            tool_filter: None,
            budget: super::super::BudgetGrant {
                max_cost_usd: Some(0.5),
                max_tokens: None,
            },
        };
        let config = worker.run_config(&task);
        assert_eq!(config.cost_budget_usd, Some(0.5));
    }

    #[test]
    fn usage_step_is_shared_with_the_port() {
        // The provider path emits Usage via the same `usage_step` mapper used by the
        // synthesized CLI stream — proving the two families' Usage steps are shaped
        // identically (the parity property).
        let step = usage_step(&nerve_agent::Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..nerve_agent::Usage::default()
        });
        assert!(matches!(
            step,
            AgentEventKind::Usage {
                input_tokens: 1,
                ..
            }
        ));
    }
}
