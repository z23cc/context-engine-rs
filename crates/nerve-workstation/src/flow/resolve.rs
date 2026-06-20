//! Worker resolution + small pure helpers for the [`Driver`](super::driver::Driver).
//!
//! Split out of `driver.rs` for the file-size convention: this module holds the
//! [`WorkerResolver`] seam (how a declarative [`WorkerRef`] becomes a runnable
//! [`AgentWorker`]) and the strategy/result helper functions the driver folds
//! with. All pure — no engine state, no nondeterminism.

use crate::worker::{
    AgentWorker, LedgerEntry, ReplayWorker, SpawnRefusal, TurnResult, WorkerError, WorkerFactory,
    WorkerKind, WorkerLedger,
};
use nerve_runtime::{Step, Strategy, WorkerRef, WorkflowDef};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::{FlowOutcome, NodeId};

/// How the driver resolves a [`WorkerRef`] into a runnable [`AgentWorker`]. The
/// production driver uses the C0 [`WorkerFactory`]; tests inject a closure that
/// hands back scripted `FakeWorker`s / `ReplayWorker`s, so the loop is testable
/// without a real LLM/subprocess.
pub(crate) trait WorkerResolver: Sync {
    /// Mint the worker for `worker_ref` (or fail before any spawn).
    fn resolve(&self, worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError>;
}

/// The production resolver: maps a [`WorkerRef`] onto a [`WorkerKind`] and mints
/// it through the C0 [`WorkerFactory`].
pub(crate) struct FactoryResolver {
    factory: WorkerFactory,
}

impl FactoryResolver {
    pub(crate) fn new(factory: WorkerFactory) -> Self {
        Self { factory }
    }
}

impl WorkerResolver for FactoryResolver {
    fn resolve(&self, worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let kind = worker_kind(worker_ref)?;
        self.factory.create(kind)
    }
}

/// The REPLAY resolver (design §3): hands every node a [`ReplayWorker`] backed by a
/// recorded tape, so the engine re-runs offline — no LLM/subprocess. Built from a
/// recorded [`WorkerLedger`] (loaded from the [`FlowStore`](crate::flow_store) for
/// `flow.replay`), which is SELF-CONTAINED: the prompt→node map is recovered from the
/// tape's `Start` entries. The companion [`replay_generation_provider`] replays each
/// node's recorded `snapshot_generation`, so the re-emitted tape is byte-identical
/// even when the record run mutated files (design §5).
pub(crate) struct ReplayResolver {
    recorded: Arc<Vec<LedgerEntry>>,
    prompt_to_node: Arc<BTreeMap<String, String>>,
}

impl ReplayResolver {
    /// Build a replay resolver from a recorded ledger: snapshot its tape and recover
    /// the rendered-prompt → node-id map from the `Start` entries.
    #[must_use]
    pub(crate) fn from_ledger(recorded: &WorkerLedger) -> Self {
        Self {
            recorded: Arc::new(recorded.snapshot()),
            prompt_to_node: Arc::new(recorded.prompt_to_node()),
        }
    }
}

impl WorkerResolver for ReplayResolver {
    fn resolve(&self, _worker_ref: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        Ok(Box::new(ReplayWorker::new(
            Arc::clone(&self.recorded),
            Arc::clone(&self.prompt_to_node),
        )))
    }
}

/// Build a per-node `snapshot_generation` provider that replays each node's RECORDED
/// generation (design §5): the engine pins the recorded value at each node-start, so
/// a node that observed a mutation-bumped generation during the record run observes
/// the SAME generation under replay — byte-identical even under file mutation. Falls
/// back to `0` for a node with no recorded generation (e.g. a node never started).
pub(crate) fn replay_generation_provider(
    recorded: &WorkerLedger,
) -> impl Fn(&WorkflowDef, &str) -> u64 + Sync + use<> {
    let generations = recorded.node_generations();
    move |_def: &WorkflowDef, node: &str| generations.get(node).copied().unwrap_or(0)
}

/// Translate a declarative [`WorkerRef`] into the C0 [`WorkerKind`] the factory
/// mints. `Named` is defined-ahead (design §6, C6) and refused here — C1 does not
/// resolve named worker defs yet.
fn worker_kind(worker_ref: &WorkerRef) -> Result<WorkerKind, WorkerError> {
    match worker_ref {
        WorkerRef::Cli { name } => {
            // The factory's `Cli` kind takes a `&'static str` catalog name; map
            // the data string onto the known catalog (an unknown name is refused
            // before any spawn, mirroring the factory's own check).
            let catalog = match name.as_str() {
                "codex" => "codex",
                "claude" => "claude",
                "gemini" => "gemini",
                other => {
                    return Err(WorkerError::Start(format!(
                        "unknown CLI worker `{other}` (expected codex|claude|gemini)"
                    )));
                }
            };
            Ok(WorkerKind::Cli(catalog))
        }
        WorkerRef::Provider { provider, model } => Ok(WorkerKind::Provider {
            provider: provider.clone(),
            model: model.clone(),
        }),
        WorkerRef::Named { name } => Err(WorkerError::Start(format!(
            "named worker `{name}` is not resolvable in C1 (WorkerDef loading lands in C6)"
        ))),
    }
}

/// Look up the declared [`Step`] for a `node` by decoding its id against `def`'s
/// strategy (design §3). Resolving by NODE ID (not a flat index) is what lets the
/// richer C5 strategies — whose nodes have heterogeneous roles (candidate vs judge,
/// map vs reduce, a debate turn, a planner, a nested child node) — share one engine
/// loop. A `Hierarchical` child node (`child/…`) is resolved against the child
/// strategy by stripping the prefix and recursing.
pub(super) fn step_for_node<'a>(def: &'a WorkflowDef, node: &NodeId) -> Option<&'a Step> {
    step_in_strategy(&def.strategy, node.as_str())
}

/// Resolve a node id against a (possibly child) strategy. Pure string decode over the
/// engine's own typed [`NodeId`] constructors, so it is total + deterministic.
fn step_in_strategy<'a>(strategy: &'a Strategy, node: &str) -> Option<&'a Step> {
    // A child-flow node delegates to the child strategy with the prefix stripped.
    if let Some(child) = node.strip_prefix(crate::flow::CHILD_PREFIX) {
        return match strategy {
            Strategy::Hierarchical { child: inner, .. } => step_in_strategy(inner, child),
            _ => None,
        };
    }
    match strategy {
        Strategy::Single { step } if node == "node-0" => Some(step),
        Strategy::Parallel { branches, .. } => {
            index_suffix(node, "branch-").and_then(|i| branches.get(i))
        }
        Strategy::Pipeline { stages } => index_suffix(node, "stage-").and_then(|i| stages.get(i)),
        Strategy::VoteJudge {
            candidates, judge, ..
        } => {
            if node == "judge" {
                Some(judge)
            } else {
                index_suffix(node, "cand-").and_then(|i| candidates.get(i))
            }
        }
        Strategy::MapReduce { map, reduce, .. } => match node {
            "reduce" => Some(reduce),
            // Every map node runs the SAME declared `map` step (one per split item).
            n if n.starts_with("map-") => Some(map),
            _ => None,
        },
        Strategy::Debate { sides, judge, .. } => {
            if node == "judge" {
                Some(judge)
            } else {
                debate_side(node).and_then(|s| sides.get(s))
            }
        }
        Strategy::Hierarchical { planner, .. } if node == "planner" => Some(planner),
        _ => None,
    }
}

/// Parse `prefix{i}` → `i` (the index suffix of a node id).
fn index_suffix(node: &str, prefix: &str) -> Option<usize> {
    node.strip_prefix(prefix).and_then(|n| n.parse().ok())
}

/// Parse a debate turn id `side-{s}-round-{r}` → `s` (which declared side ran it).
fn debate_side(node: &str) -> Option<usize> {
    node.strip_prefix("side-")
        .and_then(|rest| rest.split_once("-round-"))
        .and_then(|(side, _round)| side.parse().ok())
}

/// Whether `def`'s strategy interpolates a `{{prev}}` alias (only a `Pipeline` has
/// an ordered upstream "previous stage"). For a `Single`/`Parallel` node `prev`
/// has no meaning and is left as an unresolved placeholder.
pub(super) fn is_pipeline(def: &WorkflowDef) -> bool {
    matches!(def.strategy, Strategy::Pipeline { .. })
}

/// Whether a strategy runs a single live frontier at a time (so that frontier is
/// `flow.steer`-able, C3a). `Single` and `Pipeline` advance one node at a time; a
/// `Parallel` wave has no single live frontier and is not steerable here.
pub(super) fn is_steerable_strategy(strategy: &Strategy) -> bool {
    matches!(
        strategy,
        Strategy::Single { .. } | Strategy::Pipeline { .. }
    )
}

/// A provider worker's model override comes from the `WorkerRef`; a CLI worker
/// takes its model from config, so `None` here.
pub(super) fn provider_model(worker_ref: &WorkerRef) -> Option<String> {
    match worker_ref {
        WorkerRef::Provider { model, .. } => Some(model.clone()),
        WorkerRef::Cli { .. } | WorkerRef::Named { .. } => None,
    }
}

/// A failed turn result (a resolve/start error or a panicked thread).
pub(super) fn failed_result(reason: &str) -> TurnResult {
    TurnResult {
        ok: false,
        text: format!("worker failed: {reason}"),
        usage: nerve_agent::Usage::default(),
        cost_usd: None,
        timed_out: false,
    }
}

/// The recorded result for a node refused at a budget ceiling (design §8): a
/// not-ok result with zero usage (the node never ran), describing the refusal so
/// the audit trail + outcome are explicit. Folds like any other failure.
pub(super) fn refused_result(refusal: SpawnRefusal) -> TurnResult {
    let reason = match refusal {
        SpawnRefusal::Depth { depth, max_depth } => {
            format!("spawn refused at depth ceiling ({depth}/{max_depth})")
        }
        SpawnRefusal::Workers {
            live_workers,
            max_workers,
        } => format!("spawn refused at worker ceiling ({live_workers}/{max_workers})"),
        SpawnRefusal::Budget => "spawn refused: fleet budget exhausted".to_string(),
    };
    TurnResult {
        ok: false,
        text: reason,
        usage: nerve_agent::Usage::default(),
        cost_usd: None,
        timed_out: false,
    }
}

/// The terminal outcome for a cancelled flow (the cancel token fired).
pub(super) fn cancelled_outcome() -> FlowOutcome {
    FlowOutcome {
        ok: false,
        results: Vec::new(),
        summary: "flow cancelled".to_string(),
    }
}

/// The terminal outcome for a flow that stopped without emitting one (a defensive
/// fallback against an interpreter bug).
pub(super) fn terminated_without_emit() -> FlowOutcome {
    FlowOutcome {
        ok: false,
        results: Vec::new(),
        summary: "flow terminated without emitting an outcome".to_string(),
    }
}
