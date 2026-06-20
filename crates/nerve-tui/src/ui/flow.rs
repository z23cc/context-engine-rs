//! Pure `/flow` parsing: turn a human-friendly shorthand (or `--file`) into a
//! [`WorkflowDef`] the daemon's flow engine runs (C-TUI §1). The shorthand covers
//! the three common strategies — `parallel`, `vote`, `pipeline` — by building the
//! [`Strategy`] on the fly; the `--file <path.json>` escape hatch loads a full
//! [`WorkflowDef`] for the strategies the shorthand can't express (map-reduce /
//! debate / hierarchical).
//!
//! Pure decisions live here so the command shape is unit-testable without a live
//! client (mirroring `commands.rs` / `input/delegate.rs`'s split). The handler in
//! [`crate::app::input`] sends the resulting [`RuntimeCommand::FlowStart`].

use nerve_runtime::{
    FailPolicy, FlowSource, Join, RuntimeCommand, Step, Strategy, TaskTemplate, WorkerRef,
    WorkflowDef,
};

/// CLI worker catalog names the shorthand maps to [`WorkerRef::Cli`]; anything
/// else (or a `provider:model` spelling) becomes a [`WorkerRef::Provider`].
/// Mirrors the delegate runtime's catalog names (DA-1/DA-2).
pub const CLI_AGENTS: &[&str] = &["codex", "claude", "gemini"];

/// The schema version the shorthand stamps onto a built [`WorkflowDef`] (current
/// on-disk version is `1`).
const WORKFLOW_SCHEMA_VERSION: u32 = 1;

/// What a parsed `/flow …` line resolves to: either a built [`WorkflowDef`] (the
/// shorthand strategies), or a request to load one from a file (`--file <path>`).
/// The handler turns either into a [`RuntimeCommand::FlowStart`].
#[derive(Debug, Clone, PartialEq)]
pub enum FlowSpec {
    /// A fully-built workflow from a shorthand (`parallel`/`vote`/`pipeline`).
    Inline(Box<WorkflowDef>),
    /// Load a full [`WorkflowDef`] from a JSON file at this path.
    File(String),
}

impl FlowSpec {
    /// Whether the spec uses any CLI worker (needs the `--allow-delegate` lift).
    /// A `--file` spec is treated as possibly-CLI (the handler surfaces the
    /// daemon's clear error either way), so this only reports for the inline case.
    #[must_use]
    pub fn uses_cli_worker(&self) -> bool {
        match self {
            Self::Inline(def) => strategy_uses_cli(&def.strategy),
            Self::File(_) => true,
        }
    }

    /// Build the [`RuntimeCommand::FlowStart`] this spec maps to. A `--file` spec
    /// is resolved against the supplied `file_loader` (which reads + parses the
    /// JSON), so this stays pure and the IO lives on the [`crate::app::Shell`].
    ///
    /// # Errors
    /// Returns a user-facing hint when the file cannot be read or parsed.
    pub fn into_command(
        self,
        file_loader: impl FnOnce(&str) -> Result<WorkflowDef, String>,
    ) -> Result<RuntimeCommand, String> {
        let def = match self {
            Self::Inline(def) => *def,
            Self::File(path) => file_loader(&path)?,
        };
        Ok(RuntimeCommand::FlowStart {
            workflow: FlowSource::Inline {
                workflow: Box::new(def),
            },
            inputs: None,
            workspace: None,
        })
    }
}

/// Whether a strategy runs at least one CLI worker (recursing into the
/// defined-ahead nested strategies for completeness).
fn strategy_uses_cli(strategy: &Strategy) -> bool {
    let steps_cli = |steps: &[Step]| steps.iter().any(|s| is_cli(&s.worker));
    match strategy {
        Strategy::Single { step } => is_cli(&step.worker),
        Strategy::Parallel { branches, .. } => steps_cli(branches),
        Strategy::Pipeline { stages } => steps_cli(stages),
        Strategy::VoteJudge {
            candidates, judge, ..
        } => steps_cli(candidates) || is_cli(&judge.worker),
        Strategy::MapReduce { map, reduce, .. } => is_cli(&map.worker) || is_cli(&reduce.worker),
        Strategy::Debate { sides, judge, .. } => steps_cli(sides) || is_cli(&judge.worker),
        Strategy::Hierarchical { planner, child } => {
            is_cli(&planner.worker) || strategy_uses_cli(child)
        }
        // `Strategy` is non-exhaustive; a future variant is treated as
        // possibly-CLI (conservative — the daemon still enforces the lift).
        _ => true,
    }
}

fn is_cli(worker: &WorkerRef) -> bool {
    matches!(worker, WorkerRef::Cli { .. })
}

/// Parse the `rest` of a `/flow` command into a [`FlowSpec`]. The first token is
/// the sub-command (`parallel` | `vote` | `pipeline`) or the `--file` flag; the
/// remainder is the agent list + task (or the file path). `Err` carries a
/// user-facing usage hint so a bad spec never crashes.
///
/// `default_worker` is the session's provider/model (`provider`, `model`), used to
/// resolve a bare ambiguous agent name to a provider worker.
///
/// # Errors
/// Returns a hint when the sub-command is missing/unknown, the agent list or task
/// is empty, or `--file` lacks a path.
pub fn parse_flow(rest: &str, default_worker: (&str, &str)) -> Result<FlowSpec, String> {
    let rest = rest.trim();
    let (head, tail) = match rest.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (rest, ""),
    };
    if head.is_empty() {
        return Err(usage());
    }
    if head == "--file" {
        if tail.is_empty() {
            return Err("usage: /flow --file <path.json>".to_string());
        }
        return Ok(FlowSpec::File(tail.to_string()));
    }
    let strategy = parse_strategy(head, tail, default_worker)?;
    Ok(FlowSpec::Inline(Box::new(WorkflowDef {
        schema_version: WORKFLOW_SCHEMA_VERSION,
        name: head.to_string(),
        strategy,
        budget: nerve_runtime::BudgetSpec::default(),
        max_depth: default_max_depth(),
    })))
}

fn default_max_depth() -> u32 {
    2
}

/// Build the [`Strategy`] for a shorthand sub-command. `parallel`/`vote` split the
/// agent list on commas; `pipeline` on `>`; everything maps the task verbatim.
fn parse_strategy(sub: &str, tail: &str, default_worker: (&str, &str)) -> Result<Strategy, String> {
    match sub {
        "parallel" => {
            let (agents, task) = split_agents_task(tail, ',')?;
            Ok(Strategy::Parallel {
                branches: steps(&agents, &task, default_worker),
                join: Join::All,
            })
        }
        "vote" => {
            let (agents, task) = split_agents_task(tail, ',')?;
            // Candidates are the agents; the judge is an in-process provider worker
            // on the session's provider/model. k = majority (⌈n/2⌉) so a quorum of
            // candidates must agree before the judge adjudicates.
            let candidates = steps(&agents, &task, default_worker);
            let judge = provider_step(default_worker, &judge_task(&task));
            let k = majority(candidates.len());
            Ok(Strategy::VoteJudge {
                candidates,
                judge,
                k,
            })
        }
        "pipeline" => {
            let (agents, task) = split_agents_task(tail, '>')?;
            Ok(Strategy::Pipeline {
                stages: steps(&agents, &task, default_worker),
            })
        }
        other => Err(format!(
            "unknown flow strategy: {other} — try parallel|vote|pipeline|--file"
        )),
    }
}

/// Split `tail` into the agent list (delimited by `sep`) and the trailing task.
/// The agent list is the first whitespace-delimited token; the rest is the task.
fn split_agents_task(tail: &str, sep: char) -> Result<(Vec<String>, String), String> {
    let (agents_tok, task) = match tail.split_once(char::is_whitespace) {
        Some((agents, task)) => (agents, task.trim()),
        None => (tail, ""),
    };
    let agents: Vec<String> = agents_tok
        .split(sep)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    if agents.is_empty() {
        return Err(usage());
    }
    if task.is_empty() {
        return Err(format!(
            "usage: /flow <strategy> <agents> <task> — describe the task (agents: {})",
            agents.join(&sep.to_string())
        ));
    }
    Ok((agents, task.to_string()))
}

/// Build one [`Step`] per agent, all handed the same `task`. Each agent resolves
/// to a CLI or provider worker via [`worker_for`].
fn steps(agents: &[String], task: &str, default_worker: (&str, &str)) -> Vec<Step> {
    agents
        .iter()
        .map(|agent| step(worker_for(agent, default_worker), task))
        .collect()
}

/// A [`Step`] running `worker` on `task` with the safe-default autonomy + abort.
fn step(worker: WorkerRef, task: &str) -> Step {
    Step {
        worker,
        task: TaskTemplate::new(task),
        autonomy: nerve_runtime::DelegateAutonomy::default(),
        on_fail: FailPolicy::default(),
    }
}

/// A provider-worker [`Step`] on the session's `(provider, model)`.
fn provider_step(default_worker: (&str, &str), task: &str) -> Step {
    let (provider, model) = default_worker;
    step(
        WorkerRef::Provider {
            provider: provider.to_string(),
            model: model.to_string(),
        },
        task,
    )
}

/// Resolve an agent token to a [`WorkerRef`]. The rules (kept simple, documented):
/// - a known CLI catalog name (`claude`/`codex`/`gemini`) → [`WorkerRef::Cli`];
/// - a `provider:model` spelling → [`WorkerRef::Provider`] (split on the FIRST
///   colon, so a model id with colons keeps the rest);
/// - any other bare name → [`WorkerRef::Provider`] on the session's provider with
///   the token as the model (the "ambiguous bare name defaults to the session's
///   provider" rule).
#[must_use]
pub fn worker_for(agent: &str, default_worker: (&str, &str)) -> WorkerRef {
    let agent = agent.trim();
    if CLI_AGENTS.contains(&agent.to_ascii_lowercase().as_str()) {
        return WorkerRef::Cli {
            name: agent.to_ascii_lowercase(),
        };
    }
    if let Some((provider, model)) = agent.split_once(':') {
        return WorkerRef::Provider {
            provider: provider.to_string(),
            model: model.to_string(),
        };
    }
    WorkerRef::Provider {
        provider: default_worker.0.to_string(),
        model: agent.to_string(),
    }
}

/// The judge's task: ask it to pick the best candidate answer for `task`.
fn judge_task(task: &str) -> String {
    format!("Pick the best answer for the following task and explain why:\n{task}")
}

/// Majority quorum ⌈n/2⌉ for a vote of `n` candidates (min 1).
fn majority(n: usize) -> u32 {
    n.div_ceil(2).max(1) as u32
}

/// The `/flow` usage hint shown on a bad spec.
#[must_use]
pub fn usage() -> String {
    "usage: /flow parallel|vote|pipeline <agents> <task>  ·  /flow --file <path.json>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT: (&str, &str) = ("xai", "grok-4-fast");

    fn inline(spec: FlowSpec) -> WorkflowDef {
        match spec {
            FlowSpec::Inline(def) => *def,
            FlowSpec::File(p) => panic!("expected inline, got file {p}"),
        }
    }

    #[test]
    fn parses_parallel_into_all_join_with_cli_workers() {
        let spec = parse_flow("parallel claude,codex refactor the parser", DEFAULT)
            .expect("parse parallel");
        assert!(spec.uses_cli_worker());
        let def = inline(spec);
        match def.strategy {
            Strategy::Parallel { branches, join } => {
                assert_eq!(join, Join::All);
                assert_eq!(branches.len(), 2);
                assert_eq!(
                    branches[0].worker,
                    WorkerRef::Cli {
                        name: "claude".into()
                    }
                );
                assert_eq!(
                    branches[1].worker,
                    WorkerRef::Cli {
                        name: "codex".into()
                    }
                );
                assert_eq!(branches[0].task.prompt, "refactor the parser");
                assert_eq!(branches[1].task.prompt, "refactor the parser");
            }
            other => panic!("expected Parallel, got {other:?}"),
        }
    }

    #[test]
    fn parses_vote_into_vote_judge_with_provider_judge() {
        let spec = parse_flow("vote claude,codex,grok solve x", DEFAULT).expect("parse vote");
        let def = inline(spec);
        match def.strategy {
            Strategy::VoteJudge {
                candidates,
                judge,
                k,
            } => {
                assert_eq!(candidates.len(), 3);
                // k = majority(3) = 2.
                assert_eq!(k, 2);
                // `grok` is not a CLI catalog name → a provider worker on the
                // session provider with `grok` as the model.
                assert_eq!(
                    candidates[2].worker,
                    WorkerRef::Provider {
                        provider: "xai".into(),
                        model: "grok".into()
                    }
                );
                // The judge is an in-process provider worker on the session model.
                assert_eq!(
                    judge.worker,
                    WorkerRef::Provider {
                        provider: "xai".into(),
                        model: "grok-4-fast".into()
                    }
                );
                assert!(judge.task.prompt.contains("solve x"));
            }
            other => panic!("expected VoteJudge, got {other:?}"),
        }
    }

    #[test]
    fn parses_pipeline_splitting_on_gt() {
        let spec =
            parse_flow("pipeline claude>codex draft then review", DEFAULT).expect("parse pipeline");
        let def = inline(spec);
        match def.strategy {
            Strategy::Pipeline { stages } => {
                assert_eq!(stages.len(), 2);
                assert_eq!(
                    stages[0].worker,
                    WorkerRef::Cli {
                        name: "claude".into()
                    }
                );
                assert_eq!(
                    stages[1].worker,
                    WorkerRef::Cli {
                        name: "codex".into()
                    }
                );
                assert_eq!(stages[0].task.prompt, "draft then review");
            }
            other => panic!("expected Pipeline, got {other:?}"),
        }
    }

    #[test]
    fn provider_model_spelling_maps_to_provider_worker() {
        let worker = worker_for("openai:gpt-5", DEFAULT);
        assert_eq!(
            worker,
            WorkerRef::Provider {
                provider: "openai".into(),
                model: "gpt-5".into()
            }
        );
    }

    #[test]
    fn file_spec_round_trips_and_is_cli_conservative() {
        let spec = parse_flow("--file /tmp/wf.json", DEFAULT).expect("parse file");
        assert_eq!(spec, FlowSpec::File("/tmp/wf.json".to_string()));
        // A file spec is treated as possibly-CLI (conservative).
        assert!(spec.uses_cli_worker());
    }

    #[test]
    fn into_command_builds_flow_start_inline() {
        let spec = parse_flow("parallel claude,codex hi", DEFAULT).expect("parse");
        let command = spec
            .into_command(|_| panic!("inline spec should not load a file"))
            .expect("build command");
        match command {
            RuntimeCommand::FlowStart { workflow, .. } => match workflow {
                FlowSource::Inline { workflow } => {
                    assert!(matches!(workflow.strategy, Strategy::Parallel { .. }));
                }
                FlowSource::Named { .. } => panic!("expected inline workflow"),
            },
            other => panic!("expected FlowStart, got {}", other.name()),
        }
    }

    #[test]
    fn into_command_loads_file_via_loader() {
        let spec = FlowSpec::File("wf.json".into());
        let def = WorkflowDef {
            schema_version: 1,
            name: "loaded".into(),
            strategy: Strategy::Single {
                step: step(
                    WorkerRef::Cli {
                        name: "claude".into(),
                    },
                    "do it",
                ),
            },
            budget: nerve_runtime::BudgetSpec::default(),
            max_depth: 2,
        };
        let command = spec
            .into_command(|path| {
                assert_eq!(path, "wf.json");
                Ok(def.clone())
            })
            .expect("loaded command");
        match command {
            RuntimeCommand::FlowStart { workflow, .. } => match workflow {
                FlowSource::Inline { workflow } => assert_eq!(workflow.name, "loaded"),
                FlowSource::Named { .. } => panic!("expected inline workflow"),
            },
            other => panic!("expected FlowStart, got {}", other.name()),
        }
    }

    #[test]
    fn into_command_surfaces_file_loader_error() {
        let spec = FlowSpec::File("missing.json".into());
        let err = spec
            .into_command(|_| Err("no such file".to_string()))
            .expect_err("loader error");
        assert_eq!(err, "no such file");
    }

    #[test]
    fn bad_specs_return_usage_hints_not_panics() {
        assert!(parse_flow("", DEFAULT).is_err());
        assert!(parse_flow("   ", DEFAULT).is_err());
        // Unknown sub-command.
        let err = parse_flow("frobnicate claude task", DEFAULT).expect_err("unknown");
        assert!(err.contains("unknown flow strategy"), "{err}");
        // Missing task.
        let err = parse_flow("parallel claude", DEFAULT).expect_err("no task");
        assert!(err.contains("describe the task"), "{err}");
        // Missing file path.
        let err = parse_flow("--file", DEFAULT).expect_err("no path");
        assert!(err.contains("--file <path.json>"), "{err}");
    }

    #[test]
    fn majority_quorum_is_ceil_half() {
        assert_eq!(majority(1), 1);
        assert_eq!(majority(2), 1); // ⌈2/2⌉ = 1
        assert_eq!(majority(3), 2);
        assert_eq!(majority(4), 2);
        assert_eq!(majority(5), 3);
    }
}
