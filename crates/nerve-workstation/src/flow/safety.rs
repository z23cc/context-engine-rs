//! Flow-safety static checks (design §8) — the bounded-recursion model's front door.
//!
//! Wave C5 lands the `Hierarchical` strategy, which is the only strategy that spawns a
//! CHILD flow. Two checks keep its recursion safe, complementing the runtime
//! `FleetBudget` depth gate (absence-at-floor) and monotone de-escalation:
//!
//! 1. **Static workflow validation ([`validate_workflow`])** — run ONCE at
//!    `flow.start`, before any worker spawns. It walks the (closed, finite) strategy
//!    tree and rejects a def that can never run safely: `max_depth == 0` under a
//!    `Hierarchical` strategy (the child could never spawn — a structural mistake,
//!    surfaced at start rather than silently absent-at-floor), or a `Named` worker ref
//!    (unresolvable until the C6 loader — fail at start, not mid-flight). A future
//!    named-`WorkflowDef` loader (C6) would extend this with the genuine reference-cycle
//!    check; the inline `Strategy` enum is a finite tree, so it cannot cycle
//!    structurally — the meaningful C5 guards are the two above.
//!
//! 2. **Ancestor-instruction-hash ([`InstructionTrail`])** — the dynamic fork-loop
//!    guard for `Hierarchical`: a child flow whose planner instruction repeats an
//!    ancestor's would loop forever. The trail records each ancestor planner's rendered
//!    instruction hash; a repeat is refused (absence-at-floor) rather than spawned.
//!
//! Both are PURE + deterministic, so they replay identically.

use nerve_runtime::{Strategy, WorkflowDef};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Why a [`WorkflowDef`] was rejected at `flow.start` (design §8). A clear,
/// deterministic message the host surfaces as a `RuntimeError` / hidden-CLI error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowError {
    /// A `Hierarchical` strategy with `max_depth == 0` — its child could never spawn.
    ZeroDepthHierarchy,
    /// A `Named` worker ref (unresolvable until the C6 WorkerDef loader).
    NamedWorkerUnresolvable(String),
    /// A nested `Hierarchical` planner repeats an ANCESTOR planner's instruction — a
    /// declared fork-loop (design §8, the ancestor-instruction-hash guard). `depth` is
    /// the nesting level the repeat was found at.
    PlannerForkLoop { depth: u32 },
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDepthHierarchy => write!(
                f,
                "hierarchical workflow has max_depth=0, so its child flow could never \
                 spawn (raise max_depth or use a non-hierarchical strategy)"
            ),
            Self::NamedWorkerUnresolvable(name) => write!(
                f,
                "named worker `{name}` is not resolvable yet (the WorkerDef loader lands \
                 in C6; use an inline cli/provider worker)"
            ),
            Self::PlannerForkLoop { depth } => write!(
                f,
                "hierarchical planner at depth {depth} repeats an ancestor planner's \
                 instruction (a fork-loop); each nesting level needs a distinct plan"
            ),
        }
    }
}

/// Validate a [`WorkflowDef`] before it runs (design §8). A pure walk over the strategy
/// tree; returns the first problem found, or `Ok(())` for a runnable workflow.
pub(crate) fn validate_workflow(def: &WorkflowDef) -> Result<(), WorkflowError> {
    if matches!(def.strategy, Strategy::Hierarchical { .. }) && def.max_depth == 0 {
        return Err(WorkflowError::ZeroDepthHierarchy);
    }
    validate_strategy(&def.strategy, &InstructionTrail::new(), 0)
}

/// Walk a strategy tree, rejecting an unresolvable `Named` worker anywhere in it and a
/// `Hierarchical` planner that repeats an ancestor's instruction (the fork-loop guard).
/// `trail` carries the ancestor planner instruction hashes; `depth` the nesting level.
fn validate_strategy(
    strategy: &Strategy,
    trail: &InstructionTrail,
    depth: u32,
) -> Result<(), WorkflowError> {
    for step in strategy_steps(strategy) {
        if let nerve_runtime::WorkerRef::Named { name } = &step.worker {
            return Err(WorkflowError::NamedWorkerUnresolvable(name.clone()));
        }
    }
    // Recurse into a Hierarchical child (the only nested strategy), threading the
    // planner-instruction trail so a repeat anywhere down the chain is a fork-loop.
    if let Strategy::Hierarchical { planner, child } = strategy {
        let instruction = &planner.task.prompt;
        if trail.repeats(instruction) {
            return Err(WorkflowError::PlannerForkLoop { depth });
        }
        validate_strategy(child, &trail.extend(instruction), depth + 1)?;
    }
    Ok(())
}

/// The declared [`Step`](nerve_runtime::Step)s directly in a strategy (NOT recursing
/// into a `Hierarchical` child — the caller recurses). Total over the closed enum.
fn strategy_steps(strategy: &Strategy) -> Vec<&nerve_runtime::Step> {
    match strategy {
        Strategy::Single { step } => vec![step],
        Strategy::Parallel { branches, .. } => branches.iter().collect(),
        Strategy::Pipeline { stages } => stages.iter().collect(),
        Strategy::VoteJudge {
            candidates, judge, ..
        } => candidates.iter().chain([judge]).collect(),
        Strategy::MapReduce { map, reduce, .. } => vec![map, reduce],
        Strategy::Debate { sides, judge, .. } => sides.iter().chain([judge]).collect(),
        Strategy::Hierarchical { planner, .. } => vec![planner],
        _ => Vec::new(),
    }
}

/// The ancestor-instruction trail for the dynamic `Hierarchical` fork-loop guard
/// (design §8): a stack of each ancestor planner's rendered-instruction hash. Spawning
/// a child whose planner instruction REPEATS an ancestor's would loop, so it is refused.
#[derive(Debug, Clone, Default)]
pub(crate) struct InstructionTrail {
    hashes: Vec<u64>,
}

impl InstructionTrail {
    /// A fresh, empty trail (the flow root has no ancestors).
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether `instruction` repeats an ancestor's (a fork-loop). Deterministic — a
    /// stable content hash, so replay reaches the same verdict.
    #[must_use]
    pub(crate) fn repeats(&self, instruction: &str) -> bool {
        self.hashes.contains(&hash_instruction(instruction))
    }

    /// A child trail extending this one with `instruction` (the planner's rendered
    /// instruction). Pushed when a child flow spawns, so its descendants see it.
    #[must_use]
    pub(crate) fn extend(&self, instruction: &str) -> Self {
        let mut hashes = self.hashes.clone();
        hashes.push(hash_instruction(instruction));
        Self { hashes }
    }
}

/// A stable content hash of a rendered instruction (the fork-loop key). `DefaultHasher`
/// is deterministic for a given byte sequence within a build, which is all the trail
/// needs (it never persists across builds).
fn hash_instruction(instruction: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    instruction.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::{
        BudgetSpec, ContextSplit, DelegateAutonomy, FailPolicy, Step, TaskTemplate, WorkerRef,
    };

    fn step(worker: WorkerRef) -> Step {
        Step {
            worker,
            task: TaskTemplate::new("do it"),
            autonomy: DelegateAutonomy::ReadOnly,
            on_fail: FailPolicy::Abort,
        }
    }

    fn cli() -> WorkerRef {
        WorkerRef::Cli {
            name: "claude".into(),
        }
    }

    fn def(strategy: Strategy, max_depth: u32) -> WorkflowDef {
        WorkflowDef {
            schema_version: 1,
            name: "n".into(),
            strategy,
            budget: BudgetSpec::default(),
            max_depth,
        }
    }

    #[test]
    fn accepts_a_runnable_inline_workflow() {
        let d = def(
            Strategy::MapReduce {
                map: step(cli()),
                over: ContextSplit::Shards { n: 2 },
                reduce: step(WorkerRef::Provider {
                    provider: "xai".into(),
                    model: "grok".into(),
                }),
            },
            2,
        );
        assert_eq!(validate_workflow(&d), Ok(()));
    }

    #[test]
    fn rejects_zero_depth_hierarchy() {
        let d = def(
            Strategy::Hierarchical {
                planner: step(cli()),
                child: Box::new(Strategy::Single { step: step(cli()) }),
            },
            0,
        );
        assert_eq!(
            validate_workflow(&d),
            Err(WorkflowError::ZeroDepthHierarchy)
        );
    }

    #[test]
    fn rejects_a_named_worker_anywhere_in_the_tree() {
        // A Named ref nested inside a Hierarchical child is still caught (recursion).
        let d = def(
            Strategy::Hierarchical {
                planner: step(cli()),
                child: Box::new(Strategy::Single {
                    step: step(WorkerRef::Named {
                        name: "reviewer".into(),
                    }),
                }),
            },
            2,
        );
        assert_eq!(
            validate_workflow(&d),
            Err(WorkflowError::NamedWorkerUnresolvable("reviewer".into()))
        );
    }

    #[test]
    fn instruction_trail_detects_a_repeat() {
        let trail = InstructionTrail::new();
        assert!(!trail.repeats("plan the work"));
        let child = trail.extend("plan the work");
        // A descendant planner repeating the ancestor's instruction is a fork-loop.
        assert!(child.repeats("plan the work"));
        // A distinct instruction is fine.
        assert!(!child.repeats("plan something else"));
        // Nesting composes: the grandchild sees both ancestors.
        let grandchild = child.extend("plan something else");
        assert!(grandchild.repeats("plan the work"));
        assert!(grandchild.repeats("plan something else"));
    }
}
