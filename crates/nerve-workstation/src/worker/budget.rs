//! Per-flow budget governance (Wave C3b) — design §6 + §8.
//!
//! Two cooperating pieces, both deterministic and replayable:
//!
//! 1. [`BudgetLedger`] — a PURE FOLD over recorded [`TurnResult`] usage/cost. The
//!    engine debits each finished node into it; because it folds RECORDED usage,
//!    a replay reproduces the same running totals byte-identically (golden-
//!    testable, like the [`WorkerLedger`](super::WorkerLedger) tape it shadows).
//!    It generalizes [`CostTelemetryHook`](crate::cost::CostTelemetryHook): the
//!    hook watches a single run's `Usage` events and cancels on a USD overrun; the
//!    `BudgetLedger` aggregates the WHOLE flow tree's usage and additionally caps
//!    total tokens.
//!
//! 2. [`FleetBudget`] — the spawn-control envelope (design §8): `{ depth,
//!    max_depth, live_workers, max_workers, remaining_usd, remaining_tokens }`.
//!    Before the engine starts a node it asks [`FleetBudget::may_spawn`]; at the
//!    depth/worker/budget ceiling it refuses (absence-at-floor — a deterministic,
//!    RECORDED refusal, not a crash). A [`WorkerSemaphore`] bounds `max_workers`
//!    across the whole tree (ureq is thread-per-worker).
//!
//! Monotone capability de-escalation (design §6/§8): [`BudgetGrant::intersect`]
//! carves a child node's grant from its parent by INTERSECTING — a child can only
//! NARROW, never widen, the parent's autonomy/budget. A contract test pins this.
//!
//! ## Scope (C3b)
//!
//! This module is FLOW-SCOPED. It governs the engine's own spawn path; it does
//! NOT touch the shipped `delegate_agent` / `spawn_agent` tool guards (the §8
//! guard-unification is a deferred cleanup). Budgeting of a worker that reports no
//! usage (the `gemini` recipe is UNVERIFIED) is WORST-CASE / fail-closed: see
//! [`BudgetLedger::debit`].

use super::{BudgetGrant, TurnResult};
use nerve_runtime::{BudgetSpec, DelegateAutonomy};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The threshold (fraction of the USD limit) at which a [`BudgetLedger`] first
/// emits a warning before the hard cap. Design §6 suggests ~80%.
const WARN_FRACTION: f64 = 0.80;

/// What a debit into the [`BudgetLedger`] resolved to — the driver translates this
/// into protocol events + (on overrun) a cooperative cancel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum BudgetDecision {
    /// Within budget; nothing crossed a threshold this debit.
    Within,
    /// Spend crossed the warning threshold (e.g. 80% of the USD limit) for the
    /// first time — emit a `BudgetWarning` against `limit_usd`. Not yet exhausted.
    Warn { limit_usd: f64 },
    /// A hard ceiling (USD or tokens) was crossed — the flow must cancel
    /// cooperatively (emit `FlowDecision{budget_exhausted}` + cancel every branch).
    Exhausted,
}

/// The per-flow budget fold (design §6). A PURE FOLD over recorded usage: only the
/// engine calls [`Self::debit`], serialized through a [`Mutex`], so the running
/// totals are a deterministic function of the recorded results — replay reproduces
/// them byte-identically. Default (all-`None` [`BudgetSpec`]) = unlimited.
#[derive(Debug)]
pub(crate) struct BudgetLedger {
    spec: BudgetSpec,
    state: Mutex<BudgetState>,
}

#[derive(Debug, Default)]
struct BudgetState {
    spent_usd: f64,
    spent_tokens: u64,
    warned: bool,
    exhausted: bool,
}

/// A snapshot of the running budget totals, for emitting `BudgetUpdate` + tests.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct BudgetSnapshot {
    pub(crate) spent_usd: f64,
    pub(crate) spent_tokens: u64,
    pub(crate) exhausted: bool,
}

impl BudgetLedger {
    /// Build a ledger over `spec`. All-`None` caps nothing (unlimited — the
    /// current behaviour, so existing flow tests stay green).
    #[must_use]
    pub(crate) fn new(spec: BudgetSpec) -> Self {
        Self {
            spec,
            state: Mutex::new(BudgetState::default()),
        }
    }

    /// Debit one finished node's recorded `TurnResult` into the running totals and
    /// classify the result (design §6). PURE over the recorded result, so replay
    /// reproduces the same decision sequence byte-identically.
    ///
    /// Token spend is the node's input + output (+ cache) tokens. USD spend is the
    /// node's `cost_usd` when reported. **Fail-closed for unverified usage:** a
    /// worker that reports zero tokens (e.g. the UNVERIFIED `gemini` recipe) still
    /// counts against `max_workers` (via the [`FleetBudget`]) and, when a per-node
    /// USD limit is set but no cost is reported, is charged the worst-case per-node
    /// ceiling rather than `0` — so a silent worker can never run unbounded.
    pub(crate) fn debit(&self, result: &TurnResult) -> BudgetDecision {
        let tokens = self.node_tokens(result);
        let usd = self.node_cost(result);
        let mut state = crate::sync::lock_recover(&self.state);
        // Once exhausted, every later debit stays exhausted (idempotent brake).
        if state.exhausted {
            state.spent_usd += usd;
            state.spent_tokens += tokens;
            return BudgetDecision::Exhausted;
        }
        state.spent_usd += usd;
        state.spent_tokens += tokens;
        if self.over_hard_cap(&state) {
            state.exhausted = true;
            return BudgetDecision::Exhausted;
        }
        if let Some(limit) = self.spec.max_total_cost_usd
            && !state.warned
            && state.spent_usd >= limit * WARN_FRACTION
        {
            state.warned = true;
            return BudgetDecision::Warn { limit_usd: limit };
        }
        BudgetDecision::Within
    }

    /// The current running totals (for `BudgetUpdate` + tests).
    #[must_use]
    pub(crate) fn snapshot(&self) -> BudgetSnapshot {
        let state = crate::sync::lock_recover(&self.state);
        BudgetSnapshot {
            spent_usd: state.spent_usd,
            spent_tokens: state.spent_tokens,
            exhausted: state.exhausted,
        }
    }

    /// Whether the flow's budget has no headroom left (design §6/§8). True once a
    /// debit has tripped the hard cap, OR a capped dimension is already at/over zero
    /// remaining. Used as the LIVE pre-check before a `flow.steer` turn so a steered
    /// turn is refused on an exhausted budget — exactly like a driver-dispatched spawn
    /// (finding C). An uncapped flow is never exhausted.
    #[must_use]
    pub(crate) fn is_exhausted(&self) -> bool {
        let state = crate::sync::lock_recover(&self.state);
        if state.exhausted {
            return true;
        }
        let usd_dry = self
            .spec
            .max_total_cost_usd
            .is_some_and(|limit| state.spent_usd >= limit);
        let tokens_dry = self
            .spec
            .max_total_tokens
            .is_some_and(|limit| state.spent_tokens >= limit);
        usd_dry || tokens_dry
    }

    /// The remaining USD headroom (`None` = uncapped), for carving a [`FleetBudget`].
    #[must_use]
    pub(crate) fn remaining_usd(&self) -> Option<f64> {
        let state = crate::sync::lock_recover(&self.state);
        self.spec
            .max_total_cost_usd
            .map(|limit| (limit - state.spent_usd).max(0.0))
    }

    /// The remaining token headroom (`None` = uncapped), for carving a [`FleetBudget`].
    #[must_use]
    pub(crate) fn remaining_tokens(&self) -> Option<u64> {
        let state = crate::sync::lock_recover(&self.state);
        self.spec
            .max_total_tokens
            .map(|limit| limit.saturating_sub(state.spent_tokens))
    }

    fn over_hard_cap(&self, state: &BudgetState) -> bool {
        let over_usd = self
            .spec
            .max_total_cost_usd
            .is_some_and(|limit| state.spent_usd > limit);
        let over_tokens = self
            .spec
            .max_total_tokens
            .is_some_and(|limit| state.spent_tokens > limit);
        over_usd || over_tokens
    }

    /// The USD a node debits: its reported `cost_usd`, or — fail-closed — the
    /// per-node worst-case ceiling when a USD budget is set but the worker reported
    /// no cost (so a silent worker is never free under a budget). The worst case is
    /// the whole remaining USD budget: a single unverified worker (the `gemini`
    /// recipe) can therefore consume at most the budget and never run unbounded.
    fn node_cost(&self, result: &TurnResult) -> f64 {
        match result.cost_usd {
            Some(cost) => cost,
            // Worst-case: charge the full USD ceiling (uncapped → 0, so an
            // unbudgeted flow keeps the current free-to-run behaviour).
            None => self.spec.max_total_cost_usd.unwrap_or(0.0),
        }
    }

    /// The tokens a node debits: its reported input + output + cache usage, or —
    /// fail-closed, MIRRORING [`Self::node_cost`] — the whole token ceiling when a
    /// token budget is set but the worker reported NO tokens (`gemini` and the
    /// remote/MCP recipes can report zero usage). Without this worst-case a
    /// zero-usage worker ran FREE under a token-only `BudgetSpec`, defeating the
    /// brake; now a single silent worker can consume at most the budget and the flow
    /// self-cancels rather than running unbounded. An uncapped token budget worst-
    /// cases to `0`, so an unbudgeted flow keeps the current free-to-run behaviour.
    fn node_tokens(&self, result: &TurnResult) -> u64 {
        let reported = reported_tokens(result);
        if reported > 0 {
            return reported;
        }
        self.spec.max_total_tokens.unwrap_or(0)
    }
}

/// The spawn-control envelope threaded through [`WorkerContext`](super::WorkerContext)
/// (design §8). Cheap to clone (its caps are plain values; the live-worker count is
/// a shared [`WorkerSemaphore`]). The four-mechanism safety model lives here:
/// depth ceiling + worker ceiling (absence-at-floor), with the budget brake folded
/// in via `remaining_*` (carved from the [`BudgetLedger`]).
///
/// **Depth ceiling — authority (finding K).** At runtime the driver carries a SINGLE
/// root `FleetBudget` per flow and never advances [`Self::child`], so this envelope's
/// `depth` stays `0` and its depth check is NOT the live hierarchical-depth guard. The
/// AUTHORITATIVE depth guard is the engine's pure `step_hierarchical`
/// (`flow::engine::strategy::hierarchical`): it refuses to even emit a `StartWorker` for
/// an over-ceiling child (`child_depth >= max_depth`), recording a `DepthCeiling`
/// decision instead — so the driver never receives an over-depth node to refuse, and a
/// `child()`-derived `FleetBudget` depth check would be UNREACHABLE. `Self::child` +
/// the `depth` field are therefore defense-in-depth / the seam for a future runtime
/// per-node carve; the worker-semaphore and budget-headroom mechanisms ARE live at the
/// root (they are tree-wide, so they need no per-node carve to bind). See the
/// `validate_workflow` static check (`flow::safety`) for the start-time complement.
#[derive(Clone)]
pub(crate) struct FleetBudget {
    /// This node's depth in the flow tree (0 at the root). Advisory at runtime: the
    /// driver keeps one root budget per flow, so the authoritative hierarchical-depth
    /// guard is the engine's `step_hierarchical`, not this field (finding K).
    depth: u32,
    /// The hierarchy depth ceiling (design §8; from `WorkflowDef.max_depth`).
    max_depth: u32,
    /// Shared, process-global live-worker count + ceiling (the semaphore).
    semaphore: Arc<WorkerSemaphore>,
    /// Remaining USD headroom (`None` = uncapped) at the time this was carved.
    remaining_usd: Option<f64>,
    /// Remaining token headroom (`None` = uncapped) at the time this was carved.
    remaining_tokens: Option<u64>,
}

/// Why the engine refused to start a worker (design §8, absence-at-floor). The
/// driver records each as a `FlowDecision` and skips the spawn — a deterministic,
/// recorded refusal, never a crash.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SpawnRefusal {
    /// `depth >= max_depth` — the hierarchy ceiling.
    Depth { depth: u32, max_depth: u32 },
    /// `live_workers >= max_workers` — the process-global semaphore bound.
    Workers { live_workers: u32, max_workers: u32 },
    /// The fleet budget (USD or tokens) is exhausted.
    Budget,
}

impl FleetBudget {
    /// The root fleet budget for a flow: depth 0, the spec's `max_workers` (or
    /// unbounded), and the full `remaining_*` headroom.
    #[must_use]
    pub(crate) fn root(
        max_depth: u32,
        max_workers: Option<u32>,
        remaining_usd: Option<f64>,
        remaining_tokens: Option<u64>,
    ) -> Self {
        Self {
            depth: 0,
            max_depth,
            semaphore: Arc::new(WorkerSemaphore::new(max_workers)),
            remaining_usd,
            remaining_tokens,
        }
    }

    /// Whether a new worker may be spawned at this depth right now, or the typed
    /// [`SpawnRefusal`] (absence-at-floor). Checks depth, then the process-global
    /// worker count, then remaining budget — all deterministic given the recorded
    /// budget fold. Does NOT itself acquire a slot; the caller acquires via
    /// [`Self::acquire`] only after a positive check.
    ///
    /// The depth branch is reachable only when the ROOT is constructed at the ceiling
    /// (`max_depth == 0`, as the `depth_ceiling_refuses_spawn_at_floor` test drives it);
    /// for genuine hierarchical nesting the engine's `step_hierarchical` gates depth
    /// BEFORE dispatch (finding K — see the [`FleetBudget`] type docs), so the driver
    /// never asks `may_spawn` about an over-ceiling nested node.
    pub(crate) fn may_spawn(&self) -> Result<(), SpawnRefusal> {
        if self.depth >= self.max_depth {
            return Err(SpawnRefusal::Depth {
                depth: self.depth,
                max_depth: self.max_depth,
            });
        }
        if let Some(refusal) = self.semaphore.would_refuse() {
            return Err(refusal);
        }
        if self.remaining_usd.is_some_and(|usd| usd <= 0.0)
            || self.remaining_tokens.is_some_and(|t| t == 0)
        {
            return Err(SpawnRefusal::Budget);
        }
        Ok(())
    }

    /// Acquire a live-worker slot (RAII): the returned guard decrements the global
    /// count when dropped. Returns the typed refusal if the semaphore is full (so
    /// the caller records a `FlowDecision` and skips the spawn). A worker that
    /// reports no usage still holds a slot for its lifetime — fail-closed (design §8).
    pub(crate) fn acquire(&self) -> Result<WorkerSlot, SpawnRefusal> {
        self.semaphore.acquire()
    }

    /// Carve a child node's fleet budget from this one (design §8): depth + 1,
    /// SAME process-global semaphore (the cap is tree-wide, never per-wave), and
    /// the child's remaining headroom INTERSECTED with the parent's via
    /// `latest_*` (the live budget-ledger headroom). Monotone: a child is never
    /// more capable than its parent.
    ///
    /// Not on the runtime path today (finding K): the driver keeps a single root budget
    /// per flow, and the engine's `step_hierarchical` is the authoritative hierarchical-
    /// depth guard, so this is the seam for a future per-node runtime carve + the
    /// defense-in-depth depth model (the semaphore/budget it would carry are already
    /// tree-wide at the root). Exercised by the unit contract tests below.
    #[must_use]
    pub(crate) fn child(&self, latest_usd: Option<f64>, latest_tokens: Option<u64>) -> Self {
        Self {
            depth: self.depth + 1,
            max_depth: self.max_depth,
            semaphore: Arc::clone(&self.semaphore),
            remaining_usd: intersect_min_f64(self.remaining_usd, latest_usd),
            remaining_tokens: intersect_min_u64(self.remaining_tokens, latest_tokens),
        }
    }

    /// This node's depth (for recording / tests).
    #[must_use]
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }

    /// The current live-worker count (for tests / status).
    #[must_use]
    pub(crate) fn live_workers(&self) -> u32 {
        self.semaphore.live()
    }
}

/// A process-global worker semaphore (design §8): a single counter bounding the
/// number of in-flight workers across the WHOLE flow tree. `None` cap = unbounded.
#[derive(Debug)]
pub(crate) struct WorkerSemaphore {
    live: AtomicU32,
    max: Option<u32>,
}

impl WorkerSemaphore {
    #[must_use]
    fn new(max: Option<u32>) -> Self {
        Self {
            live: AtomicU32::new(0),
            max,
        }
    }

    /// The refusal a spawn WOULD hit right now (without acquiring), or `None` if a
    /// slot is available. Used by [`FleetBudget::may_spawn`] for the deterministic
    /// pre-check; the actual slot is taken by [`Self::acquire`].
    fn would_refuse(&self) -> Option<SpawnRefusal> {
        let max = self.max?;
        let live = self.live.load(Ordering::SeqCst);
        (live >= max).then_some(SpawnRefusal::Workers {
            live_workers: live,
            max_workers: max,
        })
    }

    /// Acquire a slot, returning a guard that releases it on drop. Atomically
    /// refuses (compare-exchange loop) if the cap is reached, so two threads racing
    /// the last slot can never both win.
    fn acquire(self: &Arc<Self>) -> Result<WorkerSlot, SpawnRefusal> {
        match self.max {
            None => {
                self.live.fetch_add(1, Ordering::SeqCst);
                Ok(WorkerSlot {
                    semaphore: Arc::clone(self),
                })
            }
            Some(max) => {
                let mut live = self.live.load(Ordering::SeqCst);
                loop {
                    if live >= max {
                        return Err(SpawnRefusal::Workers {
                            live_workers: live,
                            max_workers: max,
                        });
                    }
                    match self.live.compare_exchange_weak(
                        live,
                        live + 1,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => {
                            return Ok(WorkerSlot {
                                semaphore: Arc::clone(self),
                            });
                        }
                        Err(observed) => live = observed,
                    }
                }
            }
        }
    }

    fn live(&self) -> u32 {
        self.live.load(Ordering::SeqCst)
    }

    fn release(&self) {
        // Saturating: never wrap below zero even if release/acquire are unbalanced.
        let mut live = self.live.load(Ordering::SeqCst);
        while live > 0 {
            match self.live.compare_exchange_weak(
                live,
                live - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => live = observed,
            }
        }
    }
}

/// An RAII live-worker slot: holds one unit of the [`WorkerSemaphore`] for a
/// worker's lifetime and releases it on drop (so a panicked worker thread still
/// frees its slot).
#[derive(Debug)]
pub(crate) struct WorkerSlot {
    semaphore: Arc<WorkerSemaphore>,
}

impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.semaphore.release();
    }
}

impl BudgetGrant {
    /// Intersect this grant with a `parent`'s — the monotone de-escalation
    /// invariant (design §6/§8): a child grant can only NARROW the parent's, never
    /// widen it. Each cap becomes the tighter (smaller) of the two; an uncapped
    /// side defers to the capped one. Pinned by a contract test.
    #[must_use]
    pub(crate) fn intersect(&self, parent: &BudgetGrant) -> BudgetGrant {
        BudgetGrant {
            max_cost_usd: intersect_min_f64(self.max_cost_usd, parent.max_cost_usd),
            max_tokens: intersect_min_u64(self.max_tokens, parent.max_tokens),
        }
    }
}

/// Intersect two autonomy postures (design §6, monotone de-escalation): the child
/// is never MORE autonomous than the parent. Returns the MIN of the two on the
/// `ReadOnly < Auto < Full` ordering, so a child can only narrow.
#[must_use]
pub(crate) fn intersect_autonomy(
    child: DelegateAutonomy,
    parent: DelegateAutonomy,
) -> DelegateAutonomy {
    if autonomy_rank(child) <= autonomy_rank(parent) {
        child
    } else {
        parent
    }
}

/// Total order on autonomy from least to most capable, for the monotone
/// de-escalation intersection. (`DelegateAutonomy` is a small closed enum:
/// `ReadOnly < Edit < Full`.)
fn autonomy_rank(autonomy: DelegateAutonomy) -> u8 {
    match autonomy {
        DelegateAutonomy::ReadOnly => 0,
        DelegateAutonomy::Edit => 1,
        DelegateAutonomy::Full => 2,
    }
}

/// The tighter (smaller) of two optional USD caps; an uncapped side defers to the
/// capped one (a cap always narrows "unlimited").
fn intersect_min_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// The tighter (smaller) of two optional token caps; an uncapped side defers to the
/// capped one.
fn intersect_min_u64(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

/// The raw number of tokens a node's result REPORTS: input, output, cache reads, and
/// cache writes summed (all the tokens the worker consumed). `0` when the worker
/// reports no usage — the fail-closed worst-case is applied by
/// [`BudgetLedger::node_tokens`].
fn reported_tokens(result: &TurnResult) -> u64 {
    let u = &result.usage;
    u64::from(u.input_tokens)
        + u64::from(u.output_tokens)
        + u64::from(u.cache_read_tokens)
        + u64::from(u.cache_creation_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(usd: Option<f64>, tokens: Option<u64>, workers: Option<u32>) -> BudgetSpec {
        BudgetSpec {
            max_total_cost_usd: usd,
            max_total_tokens: tokens,
            max_workers: workers,
        }
    }

    fn result(usd: Option<f64>, input: u32, output: u32) -> TurnResult {
        TurnResult {
            ok: true,
            text: "x".into(),
            usage: nerve_agent::Usage {
                input_tokens: input,
                output_tokens: output,
                ..nerve_agent::Usage::default()
            },
            cost_usd: usd,
            timed_out: false,
        }
    }

    #[test]
    fn unlimited_budget_never_warns_or_exhausts() {
        let ledger = BudgetLedger::new(BudgetSpec::default());
        for _ in 0..100 {
            assert_eq!(
                ledger.debit(&result(Some(1000.0), 1_000_000, 1_000_000)),
                BudgetDecision::Within
            );
        }
        assert!(!ledger.snapshot().exhausted);
    }

    #[test]
    fn usd_budget_warns_then_exhausts() {
        let ledger = BudgetLedger::new(spec(Some(1.0), None, None));
        // First debit of $0.85 crosses the 80% warning threshold.
        assert_eq!(
            ledger.debit(&result(Some(0.85), 0, 0)),
            BudgetDecision::Warn { limit_usd: 1.0 }
        );
        // Warns only once.
        assert_eq!(
            ledger.debit(&result(Some(0.10), 0, 0)),
            BudgetDecision::Within
        );
        // Now $1.05 > $1.0 → exhausted.
        assert_eq!(
            ledger.debit(&result(Some(0.10), 0, 0)),
            BudgetDecision::Exhausted
        );
        assert!(ledger.snapshot().exhausted);
        // Stays exhausted on every later debit.
        assert_eq!(
            ledger.debit(&result(Some(0.0), 0, 0)),
            BudgetDecision::Exhausted
        );
    }

    #[test]
    fn token_budget_exhausts() {
        let ledger = BudgetLedger::new(spec(None, Some(10), None));
        assert_eq!(ledger.debit(&result(None, 4, 4)), BudgetDecision::Within);
        // 8 + 4 = 12 > 10 → exhausted (no USD limit, so no warn).
        assert_eq!(ledger.debit(&result(None, 2, 2)), BudgetDecision::Exhausted);
    }

    #[test]
    fn debit_is_a_pure_fold_replayed_identically() {
        // Two ledgers over the same spec fed the same recorded results produce the
        // same decision sequence + the same final snapshot (replay determinism).
        let recorded = [
            result(Some(0.30), 100, 50),
            result(Some(0.40), 200, 60),
            result(Some(0.50), 300, 70),
        ];
        let run = || {
            let ledger = BudgetLedger::new(spec(Some(1.0), Some(10_000), None));
            let decisions: Vec<BudgetDecision> = recorded.iter().map(|r| ledger.debit(r)).collect();
            (decisions, ledger.snapshot())
        };
        let (d1, s1) = run();
        let (d2, s2) = run();
        assert_eq!(d1, d2, "decision sequence is a pure fold");
        assert_eq!(s1, s2, "final snapshot is a pure fold");
    }

    #[test]
    fn no_cost_reported_is_charged_worst_case_per_node() {
        // Fail-closed: a worker that reports no cost under a USD budget is charged
        // the per-node worst case (here the whole budget), so it can't run free.
        let ledger = BudgetLedger::new(spec(Some(2.0), None, None));
        // per_node_worst_case_usd defaults to the total budget when unset.
        let decision = ledger.debit(&result(None, 0, 0));
        assert!(
            matches!(decision, BudgetDecision::Warn { .. }),
            "a no-cost node under a budget is charged the worst case, crossing the warn line"
        );
        assert!(ledger.snapshot().spent_usd > 0.0);
    }

    #[test]
    fn no_tokens_reported_is_charged_worst_case_under_a_token_budget() {
        // Finding G: a worker that reports NO tokens (e.g. the gemini recipe or a
        // remote/MCP worker) under a TOKEN-only budget must be charged the worst case
        // (the whole token ceiling), so it can never run free and the flow self-cancels.
        let ledger = BudgetLedger::new(spec(None, Some(1000), None));
        // A zero-usage worker (no cost, no tokens) is charged the full 1000-token
        // ceiling, so 1000 is not yet OVER 1000 (within) but the budget is now dry.
        assert_eq!(ledger.debit(&result(None, 0, 0)), BudgetDecision::Within);
        assert_eq!(ledger.snapshot().spent_tokens, 1000);
        // A second zero-usage worker pushes 2000 > 1000 → exhausted (the brake bites).
        assert_eq!(ledger.debit(&result(None, 0, 0)), BudgetDecision::Exhausted);
        assert!(
            ledger.snapshot().exhausted,
            "a silent worker can self-cancel"
        );
    }

    #[test]
    fn zero_usage_worker_runs_free_only_when_uncapped() {
        // The zero-regression guarantee: under NO token cap, a zero-usage worker is
        // worst-cased to 0 (free), so an unbudgeted flow keeps its current behaviour.
        let ledger = BudgetLedger::new(spec(None, None, None));
        for _ in 0..100 {
            assert_eq!(ledger.debit(&result(None, 0, 0)), BudgetDecision::Within);
        }
        assert_eq!(ledger.snapshot().spent_tokens, 0);
        assert!(!ledger.snapshot().exhausted);
    }

    #[test]
    fn reported_tokens_are_not_worst_cased() {
        // A worker that DOES report tokens debits its reported sum, never the ceiling.
        let ledger = BudgetLedger::new(spec(None, Some(1_000_000), None));
        assert_eq!(ledger.debit(&result(None, 5, 3)), BudgetDecision::Within);
        assert_eq!(
            ledger.snapshot().spent_tokens,
            8,
            "reported sum, not the cap"
        );
    }

    #[test]
    fn fleet_budget_refuses_at_depth_ceiling() {
        let fleet = FleetBudget::root(2, None, None, None);
        assert!(fleet.may_spawn().is_ok());
        let child = fleet.child(None, None); // depth 1
        assert!(child.may_spawn().is_ok());
        let grandchild = child.child(None, None); // depth 2 == max_depth
        assert_eq!(
            grandchild.may_spawn(),
            Err(SpawnRefusal::Depth {
                depth: 2,
                max_depth: 2
            })
        );
    }

    #[test]
    fn worker_semaphore_is_process_global_and_refuses_at_cap() {
        let fleet = FleetBudget::root(10, Some(2), None, None);
        let a = fleet.acquire().expect("slot 1");
        let b = fleet.acquire().expect("slot 2");
        assert_eq!(fleet.live_workers(), 2);
        // A child shares the SAME semaphore (tree-wide cap, not per-wave).
        let child = fleet.child(None, None);
        assert_eq!(
            child.acquire().expect_err("cap reached"),
            SpawnRefusal::Workers {
                live_workers: 2,
                max_workers: 2,
            }
        );
        assert_eq!(
            child.may_spawn(),
            Err(SpawnRefusal::Workers {
                live_workers: 2,
                max_workers: 2,
            })
        );
        drop(a);
        // Releasing a slot frees the cap again.
        let c = fleet.acquire().expect("slot freed");
        assert_eq!(fleet.live_workers(), 2);
        drop(b);
        drop(c);
        assert_eq!(fleet.live_workers(), 0);
    }

    #[test]
    fn fleet_budget_refuses_when_remaining_budget_is_zero() {
        let fleet = FleetBudget::root(10, None, Some(0.0), None);
        assert_eq!(fleet.may_spawn(), Err(SpawnRefusal::Budget));
        let fleet = FleetBudget::root(10, None, None, Some(0));
        assert_eq!(fleet.may_spawn(), Err(SpawnRefusal::Budget));
    }

    // ---- Monotone de-escalation (the contract) --------------------------------

    #[test]
    fn budget_grant_intersect_only_narrows() {
        let parent = BudgetGrant {
            max_cost_usd: Some(10.0),
            max_tokens: Some(1000),
        };
        // A child that ASKS for more is clamped down to the parent.
        let greedy = BudgetGrant {
            max_cost_usd: Some(100.0),
            max_tokens: Some(100_000),
        };
        let carved = greedy.intersect(&parent);
        assert_eq!(
            carved.max_cost_usd,
            Some(10.0),
            "child cannot out-spend parent"
        );
        assert_eq!(
            carved.max_tokens,
            Some(1000),
            "child cannot out-token parent"
        );
        // A child that asks for LESS keeps its tighter ask.
        let frugal = BudgetGrant {
            max_cost_usd: Some(1.0),
            max_tokens: Some(10),
        };
        let carved = frugal.intersect(&parent);
        assert_eq!(carved.max_cost_usd, Some(1.0));
        assert_eq!(carved.max_tokens, Some(10));
        // An uncapped child under a capped parent inherits the parent's cap (a
        // child can never be MORE uncapped than its parent).
        let uncapped = BudgetGrant::default();
        let carved = uncapped.intersect(&parent);
        assert_eq!(carved.max_cost_usd, Some(10.0));
        assert_eq!(carved.max_tokens, Some(1000));
    }

    #[test]
    fn autonomy_intersect_only_de_escalates() {
        use DelegateAutonomy::{Edit, Full, ReadOnly};
        // A child asking for MORE autonomy is clamped to the parent.
        assert_eq!(intersect_autonomy(Full, ReadOnly), ReadOnly);
        assert_eq!(intersect_autonomy(Full, Edit), Edit);
        // A child asking for LESS keeps its narrower posture.
        assert_eq!(intersect_autonomy(ReadOnly, Full), ReadOnly);
        assert_eq!(intersect_autonomy(Edit, Full), Edit);
        // Equal stays equal.
        assert_eq!(intersect_autonomy(Edit, Edit), Edit);
    }

    #[test]
    fn child_fleet_budget_intersects_remaining_headroom() {
        // The child's remaining budget is the MIN of its parent's and the latest
        // ledger headroom — it can only narrow.
        let parent = FleetBudget::root(10, None, Some(5.0), Some(500));
        let child = parent.child(Some(3.0), Some(800));
        assert_eq!(child.depth(), 1);
        // remaining_usd = min(5.0, 3.0) = 3.0; remaining_tokens = min(500, 800) = 500.
        assert!(child.may_spawn().is_ok());
        let starved = parent.child(Some(0.0), Some(500));
        assert_eq!(starved.may_spawn(), Err(SpawnRefusal::Budget));
    }
}
