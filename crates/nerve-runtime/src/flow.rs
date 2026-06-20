//! Flow strategies-as-data — the declarative orchestration vocabulary.
//!
//! The orchestration design (`docs/designs/agent-orchestration.md` §3) makes a
//! `Strategy` **data, never logic**: a closed, additive-versioned enum of named
//! combinators that a deterministic Rust interpreter (the C1 engine in
//! `nerve-workstation`) folds over a recorded worker tape. Arbitrary
//! branching/looping lives in a (gated) worker, never here — a VM would destroy
//! golden-testability (design §11, non-goals).
//!
//! ## Permanent home, ZERO protocol commitment (C1)
//!
//! These types live in `nerve-runtime` because that crate is the long-term
//! protocol authority for the `flow.*` family (design §4, landed in C2). **But in
//! C1 they are NOT wired to the protocol:** they are not referenced by
//! [`RuntimeCommand`](crate::RuntimeCommand), [`RuntimeEvent`](crate::RuntimeEvent),
//! [`RUNTIME_COMMAND_NAMES`](crate::RUNTIME_COMMAND_NAMES), or the exported
//! [`RuntimeProtocolSchema`](crate::protocol). The drift gate
//! (`export-runtime-protocol -- --check`) walks only the schema reachable from
//! `RuntimeProtocolSchema`, so merely defining these here adds nothing to the
//! exported protocol — verified to keep the drift check clean. C2 promotes them
//! into the schema with an additive `RUNTIME_PROTOCOL_VERSION` bump.
//!
//! ## Implemented in C1 vs. defined-ahead for C5
//!
//! C1's engine interprets only [`Strategy::Single`] and [`Strategy::Parallel`].
//! The richer variants ([`Strategy::Pipeline`] / [`Strategy::MapReduce`] /
//! [`Strategy::VoteJudge`] / [`Strategy::Debate`] / [`Strategy::Hierarchical`])
//! are defined here so the data shape is stable and C5 only fills interpreter
//! arms — additive by construction.

use serde::{Deserialize, Serialize};

/// A complete, declarative workflow: a named [`Strategy`] plus the fleet-wide
/// governance envelope (design §3). `schema_version` is bumped only on a
/// breaking change to the data shape (additive fields don't bump it).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WorkflowDef {
    /// The on-disk schema version of this workflow document (current: `1`).
    pub schema_version: u32,
    /// A human-readable name for the workflow (surfaced in logs / the ledger).
    pub name: String,
    /// The orchestration strategy the engine interprets.
    pub strategy: Strategy,
    /// Fleet budget envelope (design §6). Threaded as a placeholder in C1 — the
    /// debit/cancel loop lands in C3.
    #[serde(default)]
    pub budget: BudgetSpec,
    /// Hierarchy depth ceiling (design §8). Default 2, matching today's
    /// `DEFAULT_MAX_DEPTH`. Threaded as a placeholder in C1.
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
}

fn default_max_depth() -> u32 {
    2
}

/// Which worker runs a [`Step`]. The only place the CLI-vs-provider distinction
/// is encoded in the data (design §7); the engine resolves it through the
/// `WorkerFactory` to a kind-agnostic `AgentWorker`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkerRef {
    /// An external agentic CLI by catalog name (`codex` | `claude` | `gemini`).
    Cli { name: String },
    /// An in-process provider loop by provider + model.
    Provider { provider: String, model: String },
    /// A worker resolved from a named `WorkerDef` data file (design §6, C6).
    /// Defined ahead; the C1 engine does not resolve `Named` yet.
    Named { name: String },
}

/// One unit of orchestrated work: which worker, what task, how autonomous, and
/// what to do if it fails.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Step {
    /// Which worker runs this step.
    pub worker: WorkerRef,
    /// The task handed to the worker (with minimal named-output interpolation).
    pub task: TaskTemplate,
    /// Autonomy posture (maps to the CLI sandbox flag / provider exec capability).
    #[serde(default)]
    pub autonomy: crate::DelegateAutonomy,
    /// What the engine does when this step's worker fails.
    #[serde(default)]
    pub on_fail: FailPolicy,
}

/// The closed, additive-versioned set of orchestration combinators (design §3).
///
/// C1 implements `Single` + `Parallel`; the rest are defined-ahead for C5 so the
/// data is stable. `#[non_exhaustive]` keeps adding variants additive for
/// downstream matchers, and the engine's interpreter matches every variant
/// explicitly (returning `Terminate` with an "unimplemented strategy" outcome
/// for the not-yet-wired ones), so the totality is compile-checked.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Strategy {
    /// Run one worker and return its result (design §3). **Implemented in C1.**
    Single { step: Step },
    /// Fan out to N branches in parallel, then fold by `join` in **declared
    /// order** (design §3, the load-bearing invariant). **Implemented in C1.**
    Parallel { branches: Vec<Step>, join: Join },
    /// Run stages in sequence; stage N reads the ledger outputs of stages `< N`.
    /// Defined-ahead for C3/C5.
    Pipeline { stages: Vec<Step> },
    /// Map a step over a context split, then reduce. Defined-ahead for C5.
    MapReduce {
        map: Step,
        over: ContextSplit,
        reduce: Step,
    },
    /// Generate `candidates`, keep the top `k`, and let a `judge` pick.
    /// Defined-ahead for C5.
    VoteJudge {
        candidates: Vec<Step>,
        judge: Step,
        k: u32,
    },
    /// Multi-round debate between `sides`, adjudicated by `judge`. Defined-ahead
    /// for C5.
    Debate {
        sides: Vec<Step>,
        rounds: u32,
        judge: Step,
    },
    /// A planner emits a child strategy the engine runs bounded by `max_depth`
    /// (design §8). Defined-ahead for C5.
    Hierarchical { planner: Step, child: Box<Strategy> },
}

/// How parallel/vote results are folded — always in **declared `Step` order**,
/// never completion order (design §3, the determinism invariant).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Join {
    /// Keep every branch result (in declared order).
    All,
    /// Keep the first OK result in declared order (a branch with `ok == true`).
    FirstOk,
    /// Keep results once `n` branches succeed (the first `n` OKs in declared
    /// order); a short quorum yields whatever OKs there were.
    Quorum { n: u32 },
}

/// What the engine does when a step's worker fails (`ok == false` or errored).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FailPolicy {
    /// Stop the whole flow on this failure (the safe default).
    #[default]
    Abort,
    /// Record the failure and continue (used by `Parallel` branches).
    Continue,
    /// Retry the step up to `n` more times before applying `Abort`.
    Retry { n: u32 },
}

/// A minimal task template (design §3, open question 3): a plain prompt plus
/// optional `{{name}}` placeholders substituted from named ledger outputs. **No
/// expression language** — named-output substitution only, deliberately.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "TaskTemplateRepr", into = "TaskTemplateRepr")]
pub struct TaskTemplate {
    /// The raw prompt with optional `{{name}}` placeholders.
    pub prompt: String,
}

impl TaskTemplate {
    /// A template from a plain prompt string.
    #[must_use]
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
        }
    }

    /// Render the prompt, substituting `{{name}}` with `lookup(name)` when it
    /// returns `Some`; an unknown placeholder is left verbatim (no silent
    /// emptying). Pure and deterministic: the same inputs render identically.
    #[must_use]
    pub fn render(&self, lookup: &dyn Fn(&str) -> Option<String>) -> String {
        let mut out = String::with_capacity(self.prompt.len());
        let mut rest = self.prompt.as_str();
        while let Some(open) = rest.find("{{") {
            out.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            match after.find("}}") {
                Some(close) => {
                    let name = after[..close].trim();
                    match lookup(name) {
                        Some(value) => out.push_str(&value),
                        None => {
                            // Leave the unresolved placeholder verbatim.
                            out.push_str("{{");
                            out.push_str(&after[..close]);
                            out.push_str("}}");
                        }
                    }
                    rest = &after[close + 2..];
                }
                None => {
                    // No closing braces: emit the rest literally and stop.
                    out.push_str("{{");
                    out.push_str(after);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }
}

/// On-disk representation of a [`TaskTemplate`]: accept either a bare string
/// (`"do X"`) or an object (`{ "prompt": "do X" }`), and always serialize as the
/// object form. Keeps the common case terse without an expression language.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
enum TaskTemplateRepr {
    Prompt(String),
    Object { prompt: String },
}

impl From<TaskTemplateRepr> for TaskTemplate {
    fn from(repr: TaskTemplateRepr) -> Self {
        match repr {
            TaskTemplateRepr::Prompt(prompt) | TaskTemplateRepr::Object { prompt } => {
                Self { prompt }
            }
        }
    }
}

impl From<TaskTemplate> for TaskTemplateRepr {
    fn from(template: TaskTemplate) -> Self {
        TaskTemplateRepr::Object {
            prompt: template.prompt,
        }
    }
}

/// How a [`Strategy::MapReduce`] splits its shared context across map workers.
/// Defined-ahead for C5; C1 never reads it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextSplit {
    /// Split a build_context selection into `n` roughly equal shards.
    Shards { n: u32 },
    /// One map worker per listed path group.
    Paths { groups: Vec<Vec<String>> },
}

/// The fleet budget envelope (design §6). C1 threads it as a placeholder; C3
/// wires the deterministic debit/cancel loop. All fields optional — an omitted
/// budget caps nothing (the engine still bounds concurrency separately).
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize, Serialize)]
pub struct BudgetSpec {
    /// Total USD ceiling across the whole tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_cost_usd: Option<f64>,
    /// Total token ceiling across the whole tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_total_tokens: Option<u64>,
    /// Maximum number of workers across the whole tree (the global semaphore).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_workers: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_strategy_round_trips_with_terse_task() {
        let value = json!({
            "schema_version": 1,
            "name": "summ",
            "strategy": {
                "type": "single",
                "step": {
                    "worker": { "kind": "cli", "name": "claude" },
                    "task": "summarize the change"
                }
            }
        });
        let def: WorkflowDef = serde_json::from_value(value).expect("parse single");
        assert_eq!(def.max_depth, 2, "default max_depth applies");
        match &def.strategy {
            Strategy::Single { step } => {
                assert_eq!(step.task.prompt, "summarize the change");
                assert_eq!(step.autonomy, crate::DelegateAutonomy::ReadOnly);
                assert_eq!(step.on_fail, FailPolicy::Abort);
                assert_eq!(
                    step.worker,
                    WorkerRef::Cli {
                        name: "claude".into()
                    }
                );
            }
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn parallel_strategy_parses_branches_and_join() {
        let value = json!({
            "schema_version": 1,
            "name": "fanout",
            "strategy": {
                "type": "parallel",
                "branches": [
                    { "worker": { "kind": "cli", "name": "claude" }, "task": "a" },
                    { "worker": { "kind": "provider", "provider": "xai", "model": "grok" }, "task": "b" }
                ],
                "join": { "kind": "quorum", "n": 2 }
            }
        });
        let def: WorkflowDef = serde_json::from_value(value).expect("parse parallel");
        match &def.strategy {
            Strategy::Parallel { branches, join } => {
                assert_eq!(branches.len(), 2);
                assert_eq!(*join, Join::Quorum { n: 2 });
            }
            other => panic!("expected Parallel, got {other:?}"),
        }
    }

    #[test]
    fn task_template_renders_named_outputs_and_leaves_unknowns() {
        let template = TaskTemplate::new("use {{prior}} but not {{missing}}");
        let rendered = template.render(&|name| (name == "prior").then(|| "RESULT".to_string()));
        assert_eq!(rendered, "use RESULT but not {{missing}}");
    }

    #[test]
    fn task_template_handles_unterminated_placeholder() {
        let template = TaskTemplate::new("trailing {{open");
        let rendered = template.render(&|_| Some("x".to_string()));
        assert_eq!(rendered, "trailing {{open");
    }

    #[test]
    fn workflow_def_serializes_task_as_object_form() {
        let def = WorkflowDef {
            schema_version: 1,
            name: "n".into(),
            strategy: Strategy::Single {
                step: Step {
                    worker: WorkerRef::Cli {
                        name: "claude".into(),
                    },
                    task: TaskTemplate::new("do it"),
                    autonomy: crate::DelegateAutonomy::ReadOnly,
                    on_fail: FailPolicy::Abort,
                },
            },
            budget: BudgetSpec::default(),
            max_depth: 2,
        };
        let value = serde_json::to_value(&def).expect("serialize");
        assert_eq!(value["strategy"]["step"]["task"]["prompt"], "do it");
        // Round-trips back to an equal value.
        let back: WorkflowDef = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, def);
    }
}
