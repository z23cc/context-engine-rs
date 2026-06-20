//! Flow-safety static checks (design §8) — the bounded-recursion model's front door.
//!
//! Wave C5 lands the `Hierarchical` strategy, which is the only strategy that spawns a
//! CHILD flow. Several checks keep recursion safe, complementing the runtime
//! `FleetBudget` depth gate (absence-at-floor) and monotone de-escalation:
//!
//! 1. **Static workflow validation ([`validate_workflow`])** — run ONCE at
//!    `flow.start`, before any worker spawns. It walks the (closed, finite) strategy
//!    tree and rejects a def that can never run safely: `max_depth == 0` under a
//!    `Hierarchical` strategy (the child could never spawn — a structural mistake,
//!    surfaced at start rather than silently absent-at-floor).
//!
//! 2. **Ancestor-instruction-hash ([`InstructionTrail`])** — the fork-loop guard for
//!    `Hierarchical`: a child flow whose planner instruction repeats an ancestor's
//!    would loop forever. This is a STATIC check run ONCE at `flow.start` over the
//!    declared strategy tree — it hashes each ancestor planner's TEMPLATE instruction
//!    (`planner.task.prompt`, the unrendered template), not a per-spawn rendered string
//!    (the engine's hierarchical recursion is a pure phase machine and does not thread
//!    the trail at runtime). A nested planner whose template repeats an ancestor's is
//!    rejected at start, before any worker spawns. Since the planner template is what a
//!    `Hierarchical` def declares (named-output substitution is the driver's
//!    `build_task`, applied to a node's task — the planner prompt carries no nested
//!    per-spawn render here), the template hash is the fork-loop key the design needs.
//!
//! 3. **Reference-cycle check ([`validate_workflow_refs`], Wave C6)** — now that named
//!    workers + named workflows are *loaded* (`WorkerRegistry` / `WorkflowRegistry`)
//!    and can reference each other, a genuine graph-cycle is possible: a workflow whose
//!    `Named` reference resolves (transitively) back to the same named workflow. C6
//!    adds a real DFS over the resolved references that rejects such a cycle at
//!    `flow.start`. (C5 only had the finite-inline-tree note + the planner fork-loop
//!    hash; this is the named-reference cycle.) An unresolvable `Named` worker is also
//!    rejected here (it now RESOLVES through the registry, or errors — replacing C5's
//!    blanket "reject Named").
//!
//! All are PURE + deterministic, so they replay identically.

use super::workflow_registry::{WorkflowRegistry, reachable_named_workers};
use crate::worker::WorkerRegistry;
use nerve_runtime::{Strategy, WorkerRef, WorkflowDef};
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Why a [`WorkflowDef`] was rejected at `flow.start` (design §8). A clear,
/// deterministic message the host surfaces as a `RuntimeError` / hidden-CLI error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkflowError {
    /// A `Hierarchical` strategy with `max_depth == 0` — its child could never spawn.
    ZeroDepthHierarchy,
    /// A `Named` worker ref that does not resolve through the worker registry (C6):
    /// no `<name>.json` def in project/global/built-in. Carries the registry's reason.
    NamedWorkerUnresolvable { name: String, reason: String },
    /// A nested `Hierarchical` planner repeats an ANCESTOR planner's instruction — a
    /// declared fork-loop (design §8, the ancestor-instruction-hash guard). `depth` is
    /// the nesting level the repeat was found at.
    PlannerForkLoop { depth: u32 },
    /// A genuine reference cycle (C6): a named workflow whose `Named` references
    /// resolve (transitively) back to itself. `cycle` is the workflow-name chain.
    ReferenceCycle { cycle: Vec<String> },
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroDepthHierarchy => write!(
                f,
                "hierarchical workflow has max_depth=0, so its child flow could never \
                 spawn (raise max_depth or use a non-hierarchical strategy)"
            ),
            Self::NamedWorkerUnresolvable { name, reason } => {
                write!(f, "named worker `{name}` does not resolve: {reason}")
            }
            Self::PlannerForkLoop { depth } => write!(
                f,
                "hierarchical planner at depth {depth} repeats an ancestor planner's \
                 instruction (a fork-loop); each nesting level needs a distinct plan"
            ),
            Self::ReferenceCycle { cycle } => write!(
                f,
                "named-workflow reference cycle: {} (a workflow cannot reference itself, \
                 directly or transitively)",
                cycle.join(" -> ")
            ),
        }
    }
}

/// Validate a [`WorkflowDef`]'s STRUCTURE before it runs (design §8): a pure walk over
/// the strategy tree rejecting a zero-depth hierarchy or a planner fork-loop. It does
/// NOT resolve named refs (that needs the registries — see [`validate_workflow_refs`]);
/// the hidden `nerve flow run` CLI uses this structural check + the registry one
/// together. Returns the first problem found, or `Ok(())`.
pub(crate) fn validate_workflow(def: &WorkflowDef) -> Result<(), WorkflowError> {
    if matches!(def.strategy, Strategy::Hierarchical { .. }) && def.max_depth == 0 {
        return Err(WorkflowError::ZeroDepthHierarchy);
    }
    validate_strategy(&def.strategy, &InstructionTrail::new(), 0)
}

/// The full `flow.start` validation (C6): the structural [`validate_workflow`] checks,
/// PLUS — now that named workers + named workflows are loaded and can reference each
/// other — every `Named` worker must resolve through the [`WorkerRegistry`], and the
/// reference graph must be acyclic. Run before any worker spawns; pure + deterministic.
pub(crate) fn validate_workflow_refs(
    def: &WorkflowDef,
    workflows: &WorkflowRegistry,
    workers: &WorkerRegistry,
) -> Result<(), WorkflowError> {
    validate_workflow(def)?;
    resolve_named_workers(def, workers)?;
    // The reference-cycle DFS: the entry workflow may be anonymous (inline) — seed the
    // visited set with its own name so a Named ref BACK to it (when it is also a named
    // workflow) is caught, then follow each named ref that is itself a named workflow.
    let mut on_stack = vec![def.name.clone()];
    let mut seen = HashSet::new();
    seen.insert(def.name.clone());
    detect_reference_cycle(def, workflows, &mut on_stack, &mut seen)
}

/// Every `Named` worker in `def` must resolve through the registry (C6 turns C5's
/// blanket "reject Named" into "resolve, else error with the registry's reason").
fn resolve_named_workers(def: &WorkflowDef, workers: &WorkerRegistry) -> Result<(), WorkflowError> {
    for name in reachable_named_workers(def) {
        workers
            .resolve(&WorkerRef::Named { name: name.clone() })
            .map_err(|err| WorkflowError::NamedWorkerUnresolvable {
                name,
                reason: err.to_string(),
            })?;
    }
    Ok(())
}

/// DFS the named-workflow reference graph from `def`, detecting a cycle. An edge is a
/// `Named` reference in `def` that is ALSO a registered named workflow (a worker and a
/// workflow can share a name; following it could recurse). `on_stack` is the current
/// DFS path (for the cycle report); `seen` prunes already-cleared subtrees. A repeat of
/// a name already on the stack is a genuine cycle.
fn detect_reference_cycle(
    def: &WorkflowDef,
    workflows: &WorkflowRegistry,
    on_stack: &mut Vec<String>,
    seen: &mut HashSet<String>,
) -> Result<(), WorkflowError> {
    for name in reachable_named_workers(def) {
        // Only a Named reference that resolves to a WORKFLOW can recurse; a plain
        // worker def is a leaf (workers do not reference other workflows).
        if !workflows.contains(&name) {
            continue;
        }
        if on_stack.contains(&name) {
            let mut cycle = on_stack.clone();
            cycle.push(name);
            return Err(WorkflowError::ReferenceCycle { cycle });
        }
        if !seen.insert(name.clone()) {
            continue; // already cleared this subtree on another path
        }
        let child =
            workflows
                .resolve(&name)
                .map_err(|err| WorkflowError::NamedWorkerUnresolvable {
                    name: name.clone(),
                    reason: err.to_string(),
                })?;
        on_stack.push(name);
        detect_reference_cycle(&child, workflows, on_stack, seen)?;
        on_stack.pop();
    }
    Ok(())
}

/// Walk the declared strategy tree at `flow.start`, rejecting a `Hierarchical` planner
/// whose TEMPLATE instruction repeats an ancestor planner's (the static fork-loop
/// guard). `trail` carries the ancestor planner template-instruction hashes; `depth` the
/// nesting level. (Named-worker resolution is handled separately by
/// [`validate_workflow_refs`], so this no longer rejects `Named` refs.)
fn validate_strategy(
    strategy: &Strategy,
    trail: &InstructionTrail,
    depth: u32,
) -> Result<(), WorkflowError> {
    // Recurse into a Hierarchical child (the only nested strategy), threading the
    // planner-template-instruction trail so a repeat anywhere down the chain is a
    // fork-loop. The instruction is the planner's declared TEMPLATE prompt (this runs
    // statically at flow.start, before any render).
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

/// The ancestor-instruction trail for the STATIC `Hierarchical` fork-loop guard
/// (design §8): a stack of each ancestor planner's TEMPLATE-instruction hash, built by a
/// pure walk over the declared strategy tree at `flow.start`. A nested planner whose
/// declared template REPEATS an ancestor's would loop, so the def is rejected before any
/// worker spawns. (The check is template-level, not per-spawn rendered: the engine's
/// hierarchical recursion is a pure phase machine that does not thread this trail at
/// runtime — see [`validate_strategy`].)
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

    /// A child trail extending this one with `instruction` (the planner's declared
    /// TEMPLATE instruction). Pushed as the static walk descends into a child strategy,
    /// so a nested planner is checked against every ancestor's template.
    #[must_use]
    pub(crate) fn extend(&self, instruction: &str) -> Self {
        let mut hashes = self.hashes.clone();
        hashes.push(hash_instruction(instruction));
        Self { hashes }
    }
}

/// A stable content hash of a planner's TEMPLATE instruction (the fork-loop key).
/// `DefaultHasher` is deterministic for a given byte sequence within a build, which is
/// all the trail needs (it runs at `flow.start` and never persists across builds).
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
    fn structural_validate_no_longer_rejects_a_named_worker() {
        // C6: `validate_workflow` is STRUCTURAL only; named-worker resolution moved to
        // `validate_workflow_refs`. A bare Named ref passes the structural check.
        let d = def(
            Strategy::Single {
                step: step(WorkerRef::Named {
                    name: "reviewer".into(),
                }),
            },
            2,
        );
        assert_eq!(validate_workflow(&d), Ok(()));
    }

    #[test]
    fn refs_validation_resolves_a_builtin_named_worker() {
        // A Named ref to a built-in worker (`claude`) resolves through the registry.
        let d = def(
            Strategy::Single {
                step: step(WorkerRef::Named {
                    name: "claude".into(),
                }),
            },
            2,
        );
        let workers = WorkerRegistry::from_sources(None, None);
        let workflows = WorkflowRegistry::from_sources(None, None);
        assert_eq!(validate_workflow_refs(&d, &workflows, &workers), Ok(()));
    }

    #[test]
    fn refs_validation_rejects_an_unresolvable_named_worker() {
        let d = def(
            Strategy::Single {
                step: step(WorkerRef::Named {
                    name: "ghost".into(),
                }),
            },
            2,
        );
        let workers = WorkerRegistry::from_sources(None, None);
        let workflows = WorkflowRegistry::from_sources(None, None);
        match validate_workflow_refs(&d, &workflows, &workers) {
            Err(WorkflowError::NamedWorkerUnresolvable { name, .. }) => assert_eq!(name, "ghost"),
            other => panic!("expected unresolvable named worker, got {other:?}"),
        }
    }

    #[test]
    fn refs_validation_detects_a_self_referential_workflow_cycle() {
        // A named workflow `loop` whose strategy references a Named worker `loop` that
        // is ALSO a named workflow → a genuine self-reference cycle, rejected at start.
        let dir = tempfile::tempdir().unwrap();
        // `loop` is BOTH a worker (so the Named ref resolves) AND a workflow (so the
        // cycle DFS follows it back to itself).
        write(
            dir.path().join("workers").join("loop.json"),
            r#"{ "kind": { "type": "cli", "name": "claude" } }"#,
        );
        write(
            dir.path().join("workflows").join("loop.json"),
            r#"{ "schema_version": 1, "name": "loop", "strategy": { "type": "single", "step": { "worker": { "kind": "named", "name": "loop" }, "task": "go" } } }"#,
        );
        let workers = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let workflows = WorkflowRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let entry = workflows.resolve("loop").expect("entry workflow");
        match validate_workflow_refs(&entry, &workflows, &workers) {
            Err(WorkflowError::ReferenceCycle { cycle }) => {
                assert!(
                    cycle.first().map(String::as_str) == Some("loop"),
                    "{cycle:?}"
                );
                assert!(
                    cycle.last().map(String::as_str) == Some("loop"),
                    "{cycle:?}"
                );
            }
            other => panic!("expected a reference cycle, got {other:?}"),
        }
    }

    #[test]
    fn refs_validation_accepts_a_named_worker_that_is_not_a_workflow() {
        // A Named ref that resolves to a plain WORKER def (not a workflow) is a leaf —
        // no cycle, accepted.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path().join("workers").join("reviewer.json"),
            r#"{ "kind": { "type": "provider", "provider": "xai", "model": "grok" } }"#,
        );
        let d = def(
            Strategy::Single {
                step: step(WorkerRef::Named {
                    name: "reviewer".into(),
                }),
            },
            2,
        );
        let workers = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let workflows = WorkflowRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        assert_eq!(validate_workflow_refs(&d, &workflows, &workers), Ok(()));
    }

    /// Write a file, creating parent dirs (test helper).
    fn write(path: std::path::PathBuf, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
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
