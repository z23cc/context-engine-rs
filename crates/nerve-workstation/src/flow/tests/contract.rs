//! C4 CONTRACT tests: writer-node path-leases (design §6) + per-node snapshot-
//! generation pinning (design §5).
//!
//! These pin the two replay-fidelity-under-mutation guarantees the byte-identical
//! gate relies on: (1) two writer-nodes (Edit/Full autonomy) on overlapping path
//! scope are SERIALIZED by the engine — never concurrent — so the file-mutation
//! sequence is deterministic; (2) the `snapshot_generation` pinned at each node-start
//! is recorded in the ledger, and replay re-pins each node's RECORDED generation, so
//! a node that observed a mutation-bumped generation replays it honestly.

use super::{NeverApprover, cli_step, def};
use crate::delegate_proxy::DelegateApprover;
use crate::flow::{Driver, ReplayResolver, WorkerResolver, replay_generation_provider};
use crate::worker::{
    AgentWorker, PathLeases, TurnResult, WorkerContext, WorkerError, WorkerEvent, WorkerKind,
    WorkerLedger, WorkerSession, WorkerTask, synthesize_turn_steps,
};
use nerve_core::CancelToken;
use nerve_runtime::{
    DelegateAutonomy, FailPolicy, Join, RiskTier, Step, Strategy, TaskTemplate, WorkerRef,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---- Writer-node path-lease (the safety + replay-fidelity precondition) --------

/// A worker that records the MAX number of its kind running concurrently, so a test
/// can prove the engine serialized writers (max == 1) but let readers overlap.
struct ConcurrencyProbe {
    live: Arc<AtomicU32>,
    max_seen: Arc<AtomicU32>,
}

impl AgentWorker for ConcurrencyProbe {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Cli("probe")
    }
    fn capability(&self) -> RiskTier {
        RiskTier::Edit
    }
    fn start(
        &self,
        task: &WorkerTask,
        _ctx: &WorkerContext,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        // On entry, bump the live count and record the high-water mark; hold for a
        // beat so an unserialized sibling would overlap, then release.
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_seen.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(40));
        self.live.fetch_sub(1, Ordering::SeqCst);
        let result = TurnResult {
            ok: true,
            text: format!("did {}", task.prompt),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        };
        synthesize_turn_steps(1, &result, on_event);
        Ok(Box::new(ProbeSession { last: result }))
    }
}

struct ProbeSession {
    last: TurnResult,
}
impl WorkerSession for ProbeSession {
    fn steer(
        &mut self,
        _m: &str,
        _c: &CancelToken,
        _e: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError> {
        Err(WorkerError::NotSteerable)
    }
    fn interrupt(&self) {}
    fn close(&mut self) {}
    fn result(&self) -> TurnResult {
        self.last.clone()
    }
}

struct ProbeResolver {
    live: Arc<AtomicU32>,
    max_seen: Arc<AtomicU32>,
}
impl WorkerResolver for ProbeResolver {
    fn resolve(&self, _w: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        Ok(Box::new(ConcurrencyProbe {
            live: Arc::clone(&self.live),
            max_seen: Arc::clone(&self.max_seen),
        }))
    }
}

/// A `Parallel` of `n` writer (Edit-autonomy) steps on the same root scope.
fn writer_parallel(n: usize) -> nerve_runtime::WorkflowDef {
    let branches: Vec<Step> = (0..n)
        .map(|i| Step {
            worker: WorkerRef::Cli {
                name: "claude".into(),
            },
            task: TaskTemplate::new(format!("write {i}")),
            autonomy: DelegateAutonomy::Edit, // a WRITER node
            on_fail: FailPolicy::Continue,
        })
        .collect();
    def(
        "writers",
        Strategy::Parallel {
            branches,
            join: Join::All,
        },
    )
}

fn run_probe(
    workflow: &nerve_runtime::WorkflowDef,
    leases: Option<&PathLeases>,
    root: Option<std::path::PathBuf>,
) -> u32 {
    let live = Arc::new(AtomicU32::new(0));
    let max_seen = Arc::new(AtomicU32::new(0));
    let resolver = ProbeResolver {
        live: Arc::clone(&live),
        max_seen: Arc::clone(&max_seen),
    };
    let ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let mut driver = Driver::new(&resolver, ledger, approver, root).with_concurrency(8);
    if let Some(leases) = leases {
        driver = driver.with_leases(leases);
    }
    driver.run(workflow, &CancelToken::never());
    max_seen.load(Ordering::SeqCst)
}

#[test]
fn writer_nodes_on_overlapping_paths_are_serialized_by_the_lease() {
    // WITHOUT a lease, two writer-nodes on the same root run concurrently (max ≥ 2).
    let workflow = writer_parallel(2);
    let root = Some(std::path::PathBuf::from("/proj"));
    let unleased = run_probe(&workflow, None, root.clone());
    assert!(
        unleased >= 2,
        "without a lease the writers overlap (max concurrent = {unleased})"
    );

    // WITH a lease on the shared root scope, the engine SERIALIZES the writers: at
    // most one is ever live at a time (the safety property + replay-fidelity
    // precondition, design §6).
    let leases = PathLeases::new();
    let leased = run_probe(&workflow, Some(&leases), root);
    assert_eq!(
        leased, 1,
        "with a lease two writers on overlapping scope never overlap (max concurrent = {leased})"
    );
}

#[test]
fn readers_are_not_serialized_by_the_lease() {
    // A reader (ReadOnly autonomy) takes no lease, so a Parallel of readers still
    // overlaps even with a lease registry attached.
    let workflow = def(
        "readers",
        Strategy::Parallel {
            branches: vec![cli_step("read 0"), cli_step("read 1")], // ReadOnly
            join: Join::All,
        },
    );
    let leases = PathLeases::new();
    let max = run_probe(
        &workflow,
        Some(&leases),
        Some(std::path::PathBuf::from("/proj")),
    );
    assert!(max >= 2, "readers are not leased and overlap (max = {max})");
}

// ---- Per-node snapshot-generation pinning (design §5) --------------------------

/// A worker that records the `snapshot_generation` it was handed, so a test can
/// assert the pin was recorded and replayed.
struct GenerationProbe {
    seen: Arc<Mutex<Vec<u64>>>,
}
impl AgentWorker for GenerationProbe {
    fn kind(&self) -> WorkerKind {
        WorkerKind::Cli("gen")
    }
    fn capability(&self) -> RiskTier {
        RiskTier::ReadOnly
    }
    fn start(
        &self,
        task: &WorkerTask,
        ctx: &WorkerContext,
        _cancel: &CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError> {
        crate::sync::lock_recover(&self.seen).push(ctx.snapshot_generation);
        let result = TurnResult {
            ok: true,
            text: format!("did {}", task.prompt),
            usage: nerve_agent::Usage::default(),
            cost_usd: None,
            timed_out: false,
        };
        synthesize_turn_steps(1, &result, on_event);
        Ok(Box::new(ProbeSession { last: result }))
    }
}

struct GenerationResolver {
    seen: Arc<Mutex<Vec<u64>>>,
}
impl WorkerResolver for GenerationResolver {
    fn resolve(&self, _w: &WorkerRef) -> Result<Box<dyn AgentWorker>, WorkerError> {
        Ok(Box::new(GenerationProbe {
            seen: Arc::clone(&self.seen),
        }))
    }
}

#[test]
fn per_node_snapshot_generation_is_pinned_recorded_and_replayed() {
    // A pipeline whose generation provider BUMPS per node (simulating a node that
    // mutated files, so a later node sees a different generation). Assert the engine
    // pins what the provider returns, records it, and replay re-pins each node's
    // RECORDED generation — honest replay under mutation (design §5).
    let workflow = def(
        "gen-pipe",
        Strategy::Pipeline {
            stages: vec![
                cli_step("stage a"),
                cli_step("stage b"),
                cli_step("stage c"),
            ],
        },
    );
    // A provider that returns 10, 11, 12, … on successive node-starts.
    let counter = Arc::new(AtomicU32::new(10));
    let provider = move |_def: &nerve_runtime::WorkflowDef, _node: &str| {
        u64::from(counter.fetch_add(1, Ordering::SeqCst))
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let resolver = GenerationResolver {
        seen: Arc::clone(&seen),
    };
    let ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_generation(&provider)
        .run(&workflow, &CancelToken::never());

    // Each stage saw an INCREASING, distinct generation (the per-node pin).
    let pinned = crate::sync::lock_recover(&seen).clone();
    assert_eq!(
        pinned,
        vec![10, 11, 12],
        "each node pinned its own generation"
    );
    // And the ledger RECORDED each node's generation.
    let recorded = ledger.node_generations();
    assert_eq!(recorded.get("stage-0"), Some(&10));
    assert_eq!(recorded.get("stage-1"), Some(&11));
    assert_eq!(recorded.get("stage-2"), Some(&12));

    // REPLAY re-pins each node's RECORDED generation (NOT a fresh counter), so a
    // mutation-bumped generation replays identically. The production ReplayResolver
    // re-emits the recorded tape; the replay generation provider supplies each node's
    // recorded generation.
    let replay_resolver = ReplayResolver::from_ledger(&ledger);
    let replay_gen = replay_generation_provider(&ledger);
    let replay_ledger = Arc::new(WorkerLedger::new());
    let approver2: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    Driver::new(
        &replay_resolver,
        Arc::clone(&replay_ledger),
        approver2,
        None,
    )
    .with_generation(&replay_gen)
    .run(&workflow, &CancelToken::never());
    // The replayed tape records the SAME per-node generations as the original.
    assert_eq!(
        replay_ledger.node_generations(),
        recorded,
        "replay re-pins each node's recorded generation"
    );
}

#[test]
fn parallel_wave_pins_one_generation_for_every_branch_regardless_of_thread_timing() {
    // Finding M: every branch in a PARALLEL wave must record the SAME snapshot
    // generation, pinned ONCE on the engine thread before the wave spawns — not read
    // independently inside each concurrent branch (which made the recorded generation
    // depend on thread timing under concurrent mutation). The provider here BUMPS on
    // each call; if it were called per-branch the branches would record distinct
    // generations, but pinned per-wave they all record the SAME first value.
    let workflow = def(
        "gen-parallel",
        Strategy::Parallel {
            branches: vec![
                cli_step("branch a"),
                cli_step("branch b"),
                cli_step("branch c"),
            ],
            join: Join::All,
        },
    );
    // A provider that returns 100, 101, … on successive calls AND sleeps a touch, so a
    // per-branch (concurrent) call would interleave nondeterministically; a per-wave
    // call is made exactly once.
    let counter = Arc::new(AtomicU32::new(100));
    let counter_in = Arc::clone(&counter);
    let provider = move |_def: &nerve_runtime::WorkflowDef, _node: &str| {
        let g = counter_in.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(5));
        u64::from(g)
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let resolver = GenerationResolver {
        seen: Arc::clone(&seen),
    };
    let ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    Driver::new(&resolver, Arc::clone(&ledger), approver, None)
        .with_concurrency(8) // force all three branches to overlap
        .with_generation(&provider)
        .run(&workflow, &CancelToken::never());

    // Every branch recorded the SAME generation (the one wave pin), and the provider
    // was consulted exactly once (so the counter advanced by exactly 1).
    let recorded = ledger.node_generations();
    assert_eq!(recorded.get("branch-0"), Some(&100));
    assert_eq!(recorded.get("branch-1"), Some(&100));
    assert_eq!(recorded.get("branch-2"), Some(&100));
    assert_eq!(
        crate::sync::lock_recover(&seen).len(),
        3,
        "all three branches ran"
    );
    assert_eq!(
        counter_after(&counter),
        101,
        "the generation provider was consulted ONCE for the whole wave"
    );
}

/// Read the next value the counter would yield (without consuming), for asserting how
/// many times the per-wave generation provider was consulted.
fn counter_after(counter: &Arc<AtomicU32>) -> u32 {
    counter.load(Ordering::SeqCst)
}
