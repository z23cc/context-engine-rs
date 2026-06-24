# Agent Orchestration — the Conductor

Status: **proposed** (design) — governed by `docs/designs/architecture-north-star.md`; read that first.
Date: 2026-06-20

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc describes the `flow.*` strategies (mapreduce/debate/hierarchical/vote) as heterogeneous CANDIDATE GENERATION feeding the execution-grounded verdict — the reduce/judge node emits a Receipt-attested PR, and any vote/debate/judge tally is ADVISORY input (INV-R3), never the authoritative correctness verdict.

This is the long-term design for turning Nerve Workstation into a **super AI workspace**: a
deterministic conductor that drives a fleet of heterogeneous AI workers — external agentic CLIs
(`codex` / `claude` / `gemini`) and in-process providers (xAI-Grok / OpenAI / Anthropic with the full
Nerve tool surface) — under human approval, all over **one versioned runtime protocol**.

The thesis, stated once: **the conductor is not "another orchestrator," and the orchestration engine
is not itself the moat (2026-06-24) — the moat is the flight-recorder + execution-grounded re-verifier
(`docs/designs/trust-substrate.md`).** What the conductor buys is that its control flow is deterministic
Rust over transport-neutral data, replayable from a recorded tape, golden-testable, permission-gated,
budget-bounded, and auditable — feeding that re-verification substrate — while the workers stay
nondeterministic. Determinism of the *plan*, never of the *results*.

This design is the **convergence of the existing roadmap**, not a new subsystem. It is P3's pending
"workflow defs" given a runtime executor, governed by P4 (`PolicyToolBox` + `ApprovalHub`), persisted
by P5 (`SessionStore` discipline), surfaced by P6 (clients over the protocol). It adds **nothing to
`nerve-core`**.

---

## 1. Vision and scope

### What we are building

A **Conductor**: a host-layer orchestration engine that runs a declarative **Strategy** (parallel /
pipeline / vote-judge / map-reduce / debate / hierarchical) over a fleet of workers, where every
worker — CLI subprocess or in-process LLM loop — is reached through **one unified `AgentWorker`
port**, and the whole run is driven by an additive `flow.*` command family over the v3→v4 runtime
protocol.

Nerve already ships ~80% of the substrate, but as **two unrelated halves**:

- **External-CLI delegation** lives in `delegate_*.rs`. `delegate_live.rs` gives persistent
  bidirectional sessions (`LiveSessions` / `LiveHandle` / `LiveDriver`), `steer`/`close`, and a
  `CancelLink` that ORs a per-turn token with a session-scoped cancel. `delegate_proxy.rs` routes a
  delegated `claude`/`codex`'s own permission prompts through the shared `ApprovalHub` and back out as
  `session.respond` (DA-5b).
- **In-process agents** live in `nerve-agent` (`Orchestrator` + `LlmProvider` + `ToolBox`) plus
  `subagent.rs` (`SubAgentSpawner::run_at_depth`, the `spawn_agent` tool, and an **already-built,
  tested, but caller-less** `bounded_fan_out` primitive — `DEFAULT_FANOUT_CONCURRENCY = 4`,
  `DEFAULT_MAX_DEPTH = 2`).

The single genuinely-missing abstraction is a **unified worker port** that both halves implement, plus
a deterministic engine on top. The Conductor introduces exactly that — and nothing more.

### Scope (what this doc decides)

1. The `AgentWorker` / `WorkerSession` host port (§2).
2. The orchestration engine: strategies-as-data + a deterministic, replayable interpreter (§3).
3. Additive protocol: `flow.*` commands + `Flow*` events + `flow.replay` (§4).
4. Shared context (the `WorkerLedger`) + cross-restart persistence (§5).
5. Permission + budget/quota governance for the fleet (§6).
6. CLI-vs-provider division of labor (§7).
7. The unified recursion/safety model that replaces the two ad-hoc depth guards (§8).
8. North-star + roadmap alignment (§9), the staged build plan (§10), alternatives + risks (§11/§12).

### Non-goals (explicit)

- **No Turing-complete workflow DSL.** Strategies are a **closed, additive-versioned enum** of named
  combinators. Arbitrary branching/looping logic lives in a (gated) worker, never in the engine — a
  VM would destroy golden-testability and invite kernel-creep.
- **No distributed scheduler / no async runtime.** ureq is synchronous and thread-per-worker; a
  global worker semaphore bounds it. Revisit async only on a *measured* throughput trigger.
- **No second vector store, no new approval mechanism, no new transport.** Reuse the snapshot, the
  `ApprovalHub`, the job/event machinery, and the existing transports verbatim.
- **No new face.** The Conductor is a port + a protocol family, claimed by the `executor_for`
  totality gate — it cannot become an off-protocol entry point (the `nerve agent run` cautionary
  tale, north-star §2).

---

## 2. The unified `AgentWorker` port

One host-side port, in `nerve-workstation` (the composition layer — it touches LLM/process/wall-clock,
so it **cannot** live in `nerve-core` and is **not** protocol vocabulary). It is a **lifecycle** port,
not request/response, because both substrates already model a live, steerable session.

```rust
// crates/nerve-workstation/src/worker/mod.rs   (new)

/// What a worker is, and the only place CLI-vs-provider is visible to the engine.
pub(crate) enum WorkerKind {
    Cli(&'static str),                         // "codex" | "claude" | "gemini"
    Provider { provider: String, model: String },
}

pub(crate) struct WorkerTask {
    pub prompt: String,
    pub autonomy: nerve_runtime::DelegateAutonomy, // REUSE: maps to CLI sandbox flag / provider allow_exec
    pub model: Option<String>,
    pub tool_filter: Option<Vec<String>>,
    pub budget: BudgetGrant,                       // carved from the fleet budget (§6)
}

/// The streamed unit is the EXISTING `nerve_runtime::AgentEventKind` plus a raw
/// `Progress` line for opaque CLIs, so NO new step vocabulary is minted.
pub(crate) enum WorkerEvent {
    Step(nerve_runtime::AgentEventKind),           // TurnStarted/Message/Reasoning/Tool*/Interrupted/Usage
    Progress(String),                              // raw stdout chunk from an opaque CLI
    Approval {                                     // routed through the ONE ApprovalHub
        request_id: String, tool: String, args: serde_json::Value,
        tier: nerve_runtime::RiskTier, preview: String,
    },
}

/// The union of the shipped `DelegateOutcome` and the agent `RunOutcome`.
pub(crate) struct TurnResult {
    pub ok: bool,
    pub text: String,
    pub usage: nerve_agent::Usage,                 // input/output/cache tokens — already on both paths
    pub cost_usd: Option<f64>,
    pub timed_out: bool,
}

pub(crate) struct WorkerContext {
    pub root: Option<std::path::PathBuf>,
    pub snapshot_generation: u64,                  // pinned per node-start (§5) — replay fidelity
    pub ledger: std::sync::Arc<WorkerLedger>,      // shared blackboard + replay tape (§5)
    pub approver: std::sync::Arc<dyn crate::delegate_proxy::DelegateApprover>, // the ApprovalHub
}

pub(crate) trait AgentWorker: Send + Sync {
    fn kind(&self) -> WorkerKind;
    fn capability(&self) -> nerve_runtime::RiskTier;   // worst-case tier this worker can reach
    fn start(
        &self,
        task: &WorkerTask,
        ctx: &WorkerContext,
        cancel: &nerve_core::CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<Box<dyn WorkerSession>, WorkerError>;
}

pub(crate) trait WorkerSession: Send {
    fn steer(
        &mut self,
        message: &str,
        cancel: &nerve_core::CancelToken,
        on_event: &mut dyn FnMut(WorkerEvent),
    ) -> Result<TurnResult, WorkerError>;
    fn interrupt(&self);
    fn close(&mut self);
    fn result(&self) -> TurnResult;                // last turn's structured result + usage/cost
}
```

### How the two families implement it

**`CliWorker` — wraps the existing `LiveDriver`.** `start()` is the existing `run_delegate_live`
turn-1 + `LiveSessions::register`; `steer()` is `LiveHandle::steer`; `close()`/`interrupt()` are
`LiveHandle::request_close` (which already fires the session-scoped cancel via `CancelLink`).
`claude`/`codex` are genuinely steerable (live `DelegateSession`/`CodexSession`); `gemini` is one-shot
— modeled as a `WorkerSession` whose `steer()` returns `WorkerError::NotSteerable`. The CLI's own
`can_use_tool` permission prompts already route through `delegate_proxy.rs` to the `ApprovalHub`;
`WorkerEvent::Approval` is the same flow, re-projected. **Credentials never leak**: the env scrub in
`delegate_runtime.rs` strips `*_KEY`/`*_TOKEN`, so a CLI worker authenticates with its **own** on-disk
login — the natural quota-isolation boundary.

**`ProviderWorker` — wraps `SubAgentSpawner::run_at_depth`.** `start()` builds an `AgentRunConfig`,
resolves an `LlmProvider` via `ProviderRegistry`, and drives an `Orchestrator` over a
`RuntimeToolBox(NerveRuntime)` — i.e. exactly what `run_at_depth` assembles today, behind the **same
outermost `PolicyToolBox` gate**. The existing `AgentEvent` stream maps 1:1 to `WorkerEvent::Step`
(reuse the `map_session_agent_event` mapper already in `session_manager`). `steer()` is a new
`Orchestrator` turn on retained history — the `ResumeState`/`with_history` seam already supports this.
This is the in-process worker **with the full Nerve tool surface** (search/read/codemap/navigate/edit/
semantic on the shared snapshot).

### The property the port buys

Both families emit the **same `WorkerEvent` stream** and raise approvals through the **same
`ApprovalHub`**. So the engine, the ledger, and every client (TUI/GUI) are **worker-kind-agnostic**:
`WorkerKind` is the only place the CLI-vs-provider distinction is visible. As a bonus, the existing
`delegate_agent` and `spawn_agent` tools become thin callers of one `WorkerFactory` registry — the two
duplicate steer/park/usage/event code paths collapse into one.

---

## 3. The orchestration engine — strategies as data, control flow as deterministic Rust

### Strategies are DATA (a closed, declarative enum)

A `Strategy` is a versioned serde type in **`nerve-runtime`** (protocol authority — transport-neutral
data, drift-checked), but it is strictly **declarative**: it cannot embed logic.

```rust
// crates/nerve-runtime/src/flow.rs   (new, additive)

pub struct WorkflowDef {
    pub schema_version: u32,
    pub name: String,
    pub strategy: Strategy,
    pub budget: BudgetSpec,                 // §6
    pub max_depth: u32,                     // §8 (default 2)
}

pub enum WorkerRef {
    Cli(String),                            // "codex" | "claude" | "gemini"
    Provider { provider: String, model: String },
    Named(String),                          // resolved from a WorkerDef data file (P3, §6)
}

pub struct Step {
    pub worker: WorkerRef,
    pub task: TaskTemplate,                 // interpolates from the ledger (upstream outputs)
    pub autonomy: DelegateAutonomy,
    pub on_fail: FailPolicy,                // Abort | Continue | Retry(u32)
}

pub enum Strategy {
    Single   { step: Step },
    Parallel { branches: Vec<Step>, join: Join },          // fan-out
    Pipeline { stages: Vec<Step> },                        // stage N sees stages < N
    MapReduce { map: Step, over: ContextSplit, reduce: Step },
    VoteJudge { candidates: Vec<Step>, judge: Step, k: u32 },
    Debate   { sides: Vec<Step>, rounds: u32, judge: Step },
    Hierarchical { planner: Step, child: Box<Strategy> },  // bounded by max_depth (§8)
}

pub enum Join { All, FirstOk, Quorum(u32) }
```

`Named` workers and named `WorkflowDef`s are loaded by `Capabilities::discover` (the loader that
already discovers agent defs + skills, project > global > built-in) — **loaded, not compiled**. This
closes the one pending P3 item.

### The control flow is a deterministic interpreter

The engine lives in `crates/nerve-workstation/src/flow/engine.rs`. It is a **pure function over a
recorded tape**: a step interpreter

```
fn step(state: &FlowState, def: &WorkflowDef, ledger: &WorkerLedger) -> Vec<Action>
//                              Action ∈ { StartWorker, SteerWorker, CloseWorker,
//                                         RequestApproval, Emit, Terminate }
```

whose every transition is a pure function of `(WorkflowDef, recorded WorkerResults, recorded
approvals)`. The only nondeterminism — each worker's events, usage, cost, timing, and approval
decisions — is captured into the `WorkerLedger` (§5). Parallel fan-out reuses the **already-built**
`bounded_fan_out` primitive verbatim (this design is its first production caller).

**The single load-bearing invariant — declared-order fold.** Parallel/vote/quorum results are folded
in **declared `Step` order, never completion order** (`bounded_fan_out` already preserves input
order). So a `Parallel`/`VoteJudge` run is deterministic regardless of which worker finishes first.
A contract test pins this.

### Determinism of orchestration, golden-testability

Three modes, mirroring the kernel's golden-test discipline one layer up:

1. **RECORD** — a live run writes the `WorkerLedger` (a seq-numbered, content-addressed,
   append-only tape of every `WorkerEvent` + `TurnResult`).
2. **REPLAY** — a `ReplayWorker: AgentWorker` reads a recorded ledger and re-emits the recorded
   events instead of calling an LLM/subprocess. The engine MUST produce **byte-identical** `Flow*`
   events + the same final `FlowState`. **`flow.replay` is a first-class protocol verb (§4) and a CI
   gate** ("a recorded run must re-emit byte-identical fleet events"). This is the audit moat made
   executable.
3. **GOLDEN** — small canned ledgers per strategy (a vote with a tie, a pipeline with a mid-stage
   failure, a debate to round N, a depth-limit refusal) snapshotted with `insta`, exactly like
   `crates/nerve-core/tests/snapshots/*.snap`. A `FakeWorker` returning scripted `TurnResult`s makes
   the whole control flow snapshot-stable.

This extends — never violates — the project's determinism culture. The engine is pure; workers are the
only nondeterminism; the tape makes the run reproducible.

---

## 4. Protocol additions — additive, versioned, client-driven

All additions are **additive to the runtime protocol** (`RUNTIME_PROTOCOL_VERSION` `"3"` → `"4"`; new
serde-tagged variants, never a broken field). They are exported by `export-runtime-protocol` and
guarded by the `export-runtime-protocol -- --check` drift gate + the
`generated_protocol_rust_artifacts_are_current` test. The `delegate.*` family proved this exact
pattern; `flow.*` follows it.

### New `RuntimeCommand` variants (`command.rs` + `RUNTIME_COMMAND_NAMES` + `name()`/`tool_name()`)

| Command | Payload | Behavior |
|---|---|---|
| `flow.start` | `{ workflow: WorkflowDef \| workflow_ref: String, inputs, workspace? }` | Runs as one cancellable **job** (reuses `run_job` verbatim); `job_id` is the `flow_id`. |
| `flow.steer` | `{ flow_id, target: WorkerSelector, message }` | Inject a follow-up into a live branch (reuses `delegate.steer` plumbing per-worker). |
| `flow.respond` | `{ flow_id, request_id, decision: SessionApprovalDecision }` | **Reuses** the existing decision type + `ApprovalHub` round-trip. No new approval type. |
| `flow.get` / `flow.list` / `flow.close` | `{ flow_id }` / — / `{ flow_id }` | Mirror `session.get`/`session.list`/`session.close`. |
| `flow.replay` | `{ ledger_ref }` | Deterministic offline replay (the audit / CI verb — the differentiator). |

### New `RuntimeEvent` variants (`event.rs`; all carry `flow_id`)

- `FlowStarted { flow_id, strategy }`
- `FlowNodeStarted { flow_id, node_id, worker, kind }`  /  `FlowNodeFinished { flow_id, node_id, ok, usage }`
- `FlowEdge { flow_id, from, to }` — the DAG, for the UI
- `FlowNodeAgent { flow_id, node_id, event: AgentEventKind }` — **reuses `AgentEventKind` verbatim**,
  symmetric with the existing `SessionAgent`/`Agent`; so the TUI already knows how to render a node
  pane (it is the session pane keyed by `node_id`).
- `FlowDecision { flow_id, node_id, kind }` — the **audit trail**: a vote tally, a judge's pick, a
  debate round, a budget-exhausted event. Typed, replayable, golden-diffable.
- `BudgetUpdate { flow_id, spent_usd, tokens }` / `BudgetWarning { flow_id, spent_usd, limit_usd }`
- `FlowCompleted { flow_id, outcome }` / `FlowFailed { flow_id, node_id, error }`

**Approvals reuse `ApprovalRequested` + `flow.respond` UNCHANGED.** The `ApprovalHub` is keyed by an
id (verified: `session_id` + `request_id`); a flow branch is just another id. Add
`RuntimeEvent::session_id()` to return `flow_id` for `Flow*` events, so the existing per-id event
fan-out routing (and the existing TUI approval modal) work with **zero client change**.

### Wiring the executor — the §10 totality gate forces it

Add `Executor::Flow` to the **exhaustive** `executor_for` match in `jobs.rs` (verified at
`jobs.rs:922`, wildcard-free by design) and a `Flow` arm in `run_job`. The new variants **will not
compile** until claimed; the `executor_for_routes_each_family_to_its_owner` totality test then enforces
coverage for free. This is the structural guarantee that the Conductor **cannot** become an
off-protocol face.

### How clients drive it

Identically to sessions today: send `flow.start`, subscribe to the `flow_id`-scoped `Flow*` stream,
render the DAG from `FlowEdge`/`FlowNode*` and each node's transcript from `FlowNodeAgent`, answer
`ApprovalRequested` with `flow.respond`. No new transport, no bespoke RPC, no MCP entanglement (the MCP
face stays separate per north-star §3.5).

---

## 5. Shared context + persistence (cross-restart)

Two layers, kept orthogonal, both reusing existing patterns; **nothing in `nerve-core`**.

### Shared code context — the immutable snapshot, pinned per node

The shared read-only world is the existing **`CatalogSnapshot`** (`nerve-core`). At `flow.start` — and
critically **per node-start** — the engine pins a `snapshot_generation` into `WorkerContext`, so all
workers in a node see identical code. Pinning per node is what makes **replay honest under file
mutation**: a `CliWorker` that edits files changes the snapshot mid-run, so a later node's generation
differs; recording the generation per node-start (and treating file mutations as **recorded
artifacts**, not live FS reads, during replay) keeps REPLAY byte-identical. `build_context` assembles a
named working set; the "code blackboard" is just a pinned snapshot + a `build_context` selection — no
new store.

### Shared agent context + the audit tape — one `WorkerLedger`

The `WorkerLedger` (`crates/nerve-workstation/src/flow/ledger.rs`) is **one append-only,
content-addressed structure serving four jobs at once** (the key structural insight, grafted from the
determinism/audit proposal):

1. the **replay tape** (every `WorkerEvent` with a seq number),
2. the **cross-worker blackboard** (node outputs + artifacts that pipeline / reduce / judge steps
   read upstream),
3. the **persistence record**, and
4. the **resume source**.

Writes are serialized through the engine (only the engine writes), so they are replayable. Every write
emits a `FlowNodeAgent` / `FlowDecision` event.

### Cross-restart persistence — `FlowStore`, sibling of `SessionStore`

North-star §5 says live daemon **jobs** stay in-memory by design — but an hours-long fleet run must
survive a daemon restart. Resolution: **persist the LEDGER, not the live threads.** Add a `FlowStore`
mirroring the verified versioned `SessionStore` discipline (`SessionRecord` has `schema_version` +
`migrate_to_current` under `.nerve/sessions`):

```
.nerve/flows/<flow_id>/
  def.json            # the WorkflowDef
  ledger.jsonl        # the append-only WorkerLedger (tape = blackboard = record)
  artifacts/          # diffs / files produced by nodes
```

`FlowRecord { schema_version, migrate_to_current, ... }` is the on-disk schema. **Resume = replay,
then continue:** load the ledger, fold it through the **same interpreter** (the REPLAY path) to rebuild
scheduler + blackboard state deterministically to the last recorded node boundary, then schedule the
pending nodes live. In-process worker history resumes via the existing `ResumeState` seam; a CLI
worker's live child cannot survive process death (OS limitation), so a mid-flight CLI node is
**re-dispatched from its last recorded instruction** — never silently resumed — which is safe because
read-only is the default and writer-nodes hold path-leases (§6). Persist at **node boundaries only**,
with atomic record writes (like `SessionStore`). Promote `ledger.jsonl` → SQLite **only on a measured
trigger** (query need / write contention), per north-star invariant 8.

---

## 6. Permission + budget/quota governance for the fleet

Governance is the half ad-hoc orchestrators lack; lean in hard. **Reuse the P4 stack** — the fleet
adds aggregation, not a new mechanism (north-star invariant 9: the gate is the outermost boundary).

### Authorization — who may spawn what / touch which paths

- **Every worker's tool call still passes through the outermost `PolicyToolBox` gate.** A
  `ProviderWorker`'s toolbox stays `PolicyToolBox(DelegateAgentToolBox(ExecToolBox(...)))`; a
  `CliWorker`'s `autonomy` maps to the vendor sandbox flag via the existing `delegate_runtime`
  recipes. The fleet adds **zero new tool authority** (the same rule `fan_out` already states).
- **Spawning is itself a gated, exec-tier action.** A flow leaf = a worker spawn passes the SAME gate
  that today guards `spawn_agent` and `delegate_agent` (both classified `Ask` in `policy.rs`). So
  `PolicyToolBox` decides **who may spawn what** (e.g. policy may forbid a `Provider` worker from
  spawning a `Cli` worker). This is the precise reading of invariant 9.
- **Per-flow posture.** A flow runs under one `ApprovalMode` (reuse the shipped `AlwaysAsk`/`Write`/
  `Yolo` enum with `max_auto_tier`) and a `ProtocolApprover`; any gated call (or a CLI worker's
  proxied `can_use_tool`) raises ONE `ApprovalRequested` keyed by `flow_id` and blocks for
  `flow.respond`. `AllowAlways`/`DenyAlways` memory (the existing `DecisionMemory`) is scoped per-flow
  to fight approval fatigue. A per-node **deterministic approval timeout** (the existing
  `APPROVAL_TIMEOUT = 300s`) denies and is **recorded in the ledger** — so a deep fleet never
  deadlocks on a parked node and the deny is replayable.
- **Monotone capability de-escalation.** A child node's `autonomy` / `tool_filter` / `budget` is
  **intersected** with its parent's — a child can only narrow authority, never widen it (reuse the
  existing project-tighten-only policy precedence). Pinned by a contract test.
- **Path authority.** Each worker's `cwd` stays confined by `resolve_delegate_cwd` (the `..`-escape
  rejection) + the `SandboxLauncher` trust binding. A **writer-node path-lease** forbids two
  writer-nodes from racing on the same paths — a deterministic, engine-level check that is also the
  precondition for replay fidelity under mutation (§5).

### Budget / quota

A `BudgetLedger` carried in `WorkerContext`, **itself deterministic-replayable** (a pure fold over
recorded usage):

```
BudgetSpec { max_total_cost_usd, max_total_tokens, max_workers, max_depth, max_wall_clock, per_node? }
```

This **generalizes the existing `CostTelemetryHook`** (verified: it holds the run's cancel token and
stops cooperatively when an estimate crosses `cost_budget_usd`). Each node gets a `BudgetGrant` carved
from the parent budget; the engine debits the ledger from each node's `Usage` event (already on every
`AgentEventKind::Usage` / `DelegateUsage` / `TurnResult`), emits `BudgetUpdate`, and on overrun emits
`FlowDecision{budget_exhausted}` and **cooperatively cancels** every branch via the existing
`CancelToken` — the same mechanism `CostTelemetryHook` already uses. A buggy hierarchy is therefore
**self-terminating**: a fork bomb hits the USD ceiling and cancels. Missing usage (the `gemini` recipe
is honestly UNVERIFIED) is budgeted **worst-case / fail-closed**.

A **process-global worker semaphore** (`max_workers` across the *whole* tree, not just per-wave) bounds
ureq's thread-per-worker pressure before budget catches it.

---

## 7. CLI vs provider — division of labor

The `AgentWorker` port makes this a per-leaf **routing decision** encoded in `WorkerRef`, not an
architectural fork. The division is about **capability**, not plumbing.

**External CLIs (`CliWorker` over `codex`/`claude`/`gemini`)** — use when the subtask wants:
- a **different vendor's full agentic harness** and its own tool ecosystem (codex MCP allowlist,
  claude's own permission model);
- the worker to authenticate with its **own on-disk login** (credential/quota isolation — env scrub
  strips Nerve's secrets);
- **process isolation** for a self-contained sub-investigation, or **heterogeneity** for a
  vote/debate (codex vs claude vs gemini as independent voters).
- Heavyweight (a subprocess per worker, `NetPolicy::Allow`, 600s timeout). Steerable for
  claude/codex; one-shot for gemini.

**In-process providers (`ProviderWorker` over Grok / OpenAI / Anthropic via `LlmProvider`)** — use
when the subtask wants:
- **Nerve's own deterministic tools** (search/read/codemap/navigate/edit/semantic via
  `RuntimeToolBox`) on the **shared snapshot** (replay-friendly, zero serialization);
- **tight cost/turn control** and prompt-cache visibility (`Usage` carries cache tokens);
- **cheap, low-latency fan-out** (a thread + ureq connection, no subprocess) for map / reduce / vote
  over the same codebase.

**Rule of thumb the engine encodes:** leaf "do real coding work in a repo" nodes → CLI workers
(isolation + vendor tooling); "decide / route / summarize / vote / plan over artifacts" nodes →
in-process providers. A judge / reduce / planner step is almost always a `ProviderWorker`. The engine
sees only `AgentWorker`, so a `VoteJudge` can freely mix two CLI candidates + one in-process candidate
with an in-process judge.

---

## 8. Safety / recursion — one bounded model replacing two ad-hoc guards

**Today there are two inconsistent guards** (verified): `delegate_agent` is enabled only at
`allow_delegate && depth == 0` (`subagent.rs::wrap_exec_delegate_gate`, `delegate_tool.rs` top-level
only), while `spawn_agent` allows nesting to `DEFAULT_MAX_DEPTH = 2` (`SubAgentToolBox::may_spawn`).
The Conductor unifies them into **one explicit, data-governed `FleetBudget`** carried in
`WorkerContext`:

```
FleetBudget { depth, max_depth, live_workers, max_workers, remaining_usd, remaining_tokens }
```

Every spawn — a flow leaf, a `spawn_agent`, OR a `delegate_agent` — decrements and checks it. Safety is
the **composition** of four mechanisms, not a flat ban:

1. **Depth ceiling.** A `Hierarchical` strategy's planner emits sub-strategies the engine runs as
   child flows, but only while `depth < max_depth` (default 2, matching today). At the floor the
   flow/spawn/delegate tools are **simply not advertised** — *absence-at-floor, not error-after-call*
   (mirrors the existing `may_spawn()` pattern). Hierarchy becomes **safe-by-construction** and
   **visible in the ledger**, not a scattered `depth == 0` special-case.
2. **Monotone de-escalation.** A child is strictly *less* capable than its parent (§6) — so nested
   delegation is safe because the child cannot out-privilege or out-spend its parent, replacing the
   blunt "depth 0 only" rule with a principled invariant.
3. **Budget as the real brake.** Total cost/tokens/workers are one shared ledger across the whole
   tree; runaway recursion self-terminates by exhausting budget and cancelling via `CancelToken`.
4. **CLI workers stay structurally non-recursive by default.** A delegated `codex`/`claude` gets **no
   Nerve toolbox at all** (it is a subprocess), so the dangerous "external agent spawns external
   agents" path is **structurally impossible** unless an operator explicitly hands a CLI worker an
   MCP-exposed flow tool — a deliberate, policy-gated choice (security before openness, north-star
   §9). Plus a `WorkflowDef`-cycle check (reject cycles at `flow.start`) and an ancestor-instruction
   hash check for dynamic `Hierarchical` spawns prevent fork-loops.

Net: **budget + gate + monotone de-escalation + absence-at-floor** — strictly safer than today's
boolean *and* it actually enables the hierarchy the flagship needs.

---

## 9. North-star + roadmap alignment

This is the **convergence** of the existing roadmap, touching only the layers sanctioned for
non-determinism.

- **Prime directive (§2).** The Conductor enters through exactly two declared seams — the host
  `AgentWorker` port and the additive `flow.*` `RuntimeCommand` family — claimed by the compile-time
  `executor_for` partition (§10). `flow.start` is a job exactly like `agent.run`/`delegate.start`. It
  **retires** the `depth == 0` guard (the current cautionary shortcut) rather than adding a face.
- **Determinism boundary (invariant 1).** Nothing lands in `nerve-core`. `Strategy`/`WorkflowDef` are
  data in `nerve-runtime`; the engine, workers, ledger, and budget live in `nerve-workstation`. The
  kernel stays pure and golden-tested; the engine's own determinism is golden-tested with `FakeWorker`
  + `ReplayWorker`, extending the kernel's `insta` discipline one layer up.
- **Single dispatch hub (invariant 2).** Every worker tool call still flows through `Runtime` via
  `RuntimeToolBox`; the engine reaches tools only through that port.
- **Single protocol authority (invariants 3/4).** `flow.*` vocabulary is defined **only** in
  `nerve-runtime` as transport-neutral data, codegen'd + drift-checked, additive v3→v4; it reuses
  `AgentEventKind` / `ApprovalRequested` / `SessionApprovalDecision` and never carries domain/agent
  types. `nerve-runtime` still depends only on `nerve-core`. The MCP face stays separate (invariant 5).
- **Composition only in the binary (invariant 6).** The port, engine, `FlowStore`, ledger, budget,
  and worker factories all live in `nerve-workstation`.
- **Permission gate outermost + orthogonal containment (invariant 9).** Preserved fleet-wide:
  `PolicyToolBox` authorizes every spawn and every tool call; `SandboxLauncher` (trust-bound)
  contains each CLI worker.
- **Roadmap homes.** It **completes the one pending P3 item** (workflow defs via
  `Capabilities::discover`, loaded-not-compiled, versioned); **reuses P4** (`PolicyToolBox` +
  `ApprovalMode` + `ApprovalHub` + `CostTelemetryHook`) for governance; **reuses the P5
  `SessionStore` discipline** (`FlowStore`) for the ledger while honoring "live jobs stay in-memory by
  design"; **drives over P6** clients; and promotes the **already-built `bounded_fan_out`** to its
  first production caller. The leanest viable layer: one port + a closed strategy enum + a `flow.*`
  family + one ledger, reusing six shipped subsystems.

---

## 10. Staged build plan (de-risked; each wave independently valuable)

Each wave maps to DA-style increments and ships value alone. The spine de-risks the **abstraction
first**, then hardens the **engine off-protocol**, then exposes the **protocol last** — so we never
freeze a protocol shape we regret ("versioned or dead", north-star §9).

- **Wave C0 — the port (pure refactor, ZERO new protocol).** Extract `AgentWorker` /
  `WorkerSession` / `WorkerEvent` / `TurnResult` in `crates/nerve-workstation/src/worker/`.
  Implement `CliWorker` over the existing `LiveDriver`/`LiveSessions` and `ProviderWorker` over
  `run_at_depth`. Re-express `delegate_agent` + `spawn_agent` as thin callers of one `WorkerFactory`
  registry, collapsing the two recursion guards at the source. **Behavior-identical**, proven by the
  existing delegate/subagent tests staying green + one new golden test asserting both families emit the
  same `WorkerEvent` stream for a canned tape. Keystone everything builds on.

- **Wave C1 — engine + ledger behind a HIDDEN CLI subcommand (still no protocol).** Build the
  deterministic interpreter, the `WorkerLedger`, and `Strategy::Single` + `Strategy::Parallel` only
  (reuse `bounded_fan_out`). Drive it from a hidden `nerve flow run` subcommand so the engine hardens
  with **zero protocol commitment**. Land the `FakeWorker`/`ReplayWorker` golden tests. *Early
  flagship demo: "run the same task across claude + codex + grok, collect all results."*

- **Wave C2 — the additive protocol (= P3 workflow-defs surface). [SHIPPED]** Added `flow.start`/
  `flow.get`/`flow.list`/`flow.close`/`flow.respond` + `FlowStarted`/`FlowNode*`/`FlowEdge`/
  `FlowNodeAgent`/`FlowCompleted`/`FlowFailed` + `Executor::Flow` (the totality test forced the
  wiring), bumping `RUNTIME_PROTOCOL_VERSION` `"3"` → `"4"`; `docs/protocol/*` regenerated, drift +
  round-trip tests green. Reused `ApprovalRequested` + added `flow.respond`; `session_id() → flow_id`
  routes per-id fan-out + the existing approval modal with zero client rework. `flow.start` runs the
  C1 engine as one cancellable daemon job (`crate::flow_job`; `job_id` == `flow_id`). **Deferred to a
  later wave:** loading *named* `WorkflowDef`s via `Capabilities::discover` — the protocol carries the
  inline-or-`workflow_ref` shape, but a `workflow_ref` is refused with a clear message until the P3
  workflow-def loader lands (the inline `workflow` path is fully wired + tested).

- **Wave C3 — pipeline + shared context + steer/close + budget.** Add `Strategy::Pipeline` (stage N
  reads the ledger), `flow.steer`/`flow.close` for live branches (reuse `LiveSessions` teardown), the
  per-flow `BudgetLedger` (generalizing `CostTelemetryHook`) with `BudgetUpdate`/`BudgetWarning` +
  cooperative cancel, and the unified `FleetBudget` recursion model (depth/`max_workers`/
  policy-gated-spawn) replacing the two guards. Spawn becomes a gated exec-tier action.
  - **C3a [SHIPPED]:** `Strategy::Pipeline` (sequential stages; stage N reads upstream outputs from the
    ledger blackboard) + `flow.steer`. **Pipeline interpolation** is named-output substitution only (no
    expression language, design §12 q3): a stage's `TaskTemplate` resolves `{{<node-id>}}` (`{{stage-0}}`,
    `{{node-0}}`, `{{branch-1}}`) from any finished node's recorded output, plus a pipeline-only
    `{{prev}}` alias for the immediately-upstream stage. `flow.steer { flow_id, target: WorkerSelector,
    message }` (additive, v4 unchanged) runs one more turn on a live frontier via the C0
    `WorkerSession::steer` port through a per-flow live-flow worker registry (`SteerRegistry`,
    analogous to `LiveSessions`); `WorkerSelector { node_id? }` targets a node by id or the only live
    worker. Only `Single`/`Pipeline` frontiers are steerable; a `Parallel` wave, a one-shot worker
    (`gemini` → `NotSteerable`), a closed/advanced frontier, or an ambiguous unset selector errors
    cleanly. The steered turn is recorded into the same ledger (recorded nondeterminism, §5). Pipeline
    edges (`flow → stage-0 → stage-1 → …`) emit at `flow.start`. **Deferred to C3b:** the
    `BudgetLedger` + `FleetBudget` governance.

- **Wave C4 — replay verb + audit gate (lock the moat). [SHIPPED]** Added `flow.replay
  { ledger_ref }` (additive, v4 unchanged; `LedgerRef` is `{ flow_id }` | `{ ledger_path }`, claimed by
  the `executor_for` totality gate) + the byte-identical REPLAY CI gate (named
  `replay_is_byte_identical_*` over `Single`/`Parallel`-out-of-order/`Pipeline`/interpolating-pipeline,
  driven by the PRODUCTION `ReplayResolver` over a recorded `WorkerLedger`). The `FlowDecision` audit
  events + declared-order-fold + capability-de-escalation contract tests + the process-global worker
  semaphore landed in C3b; C4 adds the writer-node path-lease + per-node snapshot-generation pinning
  contract tests. The `WorkerLedger` now records a `Start { prompt, snapshot_generation }` per node so
  replay is SELF-CONTAINED from the persisted tape (the rendered-prompt→node map + per-node generation
  are recovered from it) — which also caught + fixed a latent bug: a `WorkerEvent::Progress(String)`
  newtype can't carry the internally-tagged `event` field, so a CLI-worker ledger never round-tripped
  (now `Progress { text }`, pinned by `every_worker_event_variant_round_trips_through_jsonl`). Added the
  `FlowStore` (versioned `FlowRecord` under `.nerve/flows/<flow_id>/{def.json, ledger.jsonl,
  record.json, artifacts/}`, atomic temp+rename writes at node boundaries) for post-hoc inspection
  (`flow.get`/`flow.list` fall back to it for finished flows) + replay. **Resume:** the deterministic
  half — `replay_to_boundary` (fold the recorded tape through the replay path) + `pending_nodes` (a pure
  `WorkflowDef`+finished-set computation) — ships with tests; LIVE re-dispatch of the pending nodes is
  the documented follow-on (design open question 1).

- **Wave C5 — richer strategies.** `Strategy::VoteJudge` + `Strategy::MapReduce` (judge/reduce are
  `ProviderWorker` leaves), then `Strategy::Debate` + `Strategy::Hierarchical` (the bounded-recursion
  model lands here). Each is data + an interpreter case + a replay golden test. Mixed-substrate nodes
  (CLI candidates + in-process judge) become demonstrable.

- **Wave C6 (defer until measured) — registry promotion + multi-client + remote/MCP workers.**
  Promote the `WorkerFactory` to a first-class `WorkerRegistry` resolving `WorkerDef` data files
  (project > global > built-in, via `Capabilities::discover`), so "add any worker" is a data change.
  Render the DAG in GUI/mobile over the same `flow.*` protocol. Add a `RemoteWorker` and an
  MCP-client-backed worker as further `AgentWorker` adapters (proving the port's generality with zero
  engine change). **Governance gate:** policy + budget (C3/C4) MUST be in place before any registry-
  driven remote/MCP worker is enabled (security before openness, §9).

### Explicitly deferred (build only on a measured trigger)

SQLite-backed ledger; resume of *in-flight* (not node-boundary) branches; a distributed scheduler; an
async runtime; user-defined control flow / a Turing-complete DSL; remote/MCP workers ahead of fleet
governance.

---

## 11. Alternatives considered

Four proposals were evaluated; all four converged on the same skeleton (one `AgentWorker` port,
strategies-as-data, an additive command family claimed by `executor_for`, kernel untouched). The
decision turned on execution discipline and the determinism/audit emphasis. This design is the
**synthesis**.

- **A — Minimalist seam (`flow.*` {start,steer,close} + one event; defer the DSL).** Lowest risk;
  best Wave-0/Wave-1 value; correctly named `bounded_fan_out` as the production caller and the
  `FleetBudget` + absence-at-floor recursion model. **Rejected as the whole answer** because it defers
  the data-driven worker registry (the P3 convergence) to a tail wave and lacks a first-class
  replay/audit verb. **Grafted:** the Wave-0 pure-refactor de-risking, absence-at-floor, and the
  unified `FleetBudget`.

- **B — Platform (`WorkerRegistry` + DSL-as-data + remote/MCP workers + 7 commands/7 events).** Most
  extensible; sharpest security-before-openness ordering; best heterogeneity story. **Rejected as the
  starting point** for the largest scope and the broadest new vocabulary (closest to the "no
  kitchen-sink protocol" anti-goal), and for committing to resumable persistence + the full strategy
  set before usage validates the shape. **Grafted:** the `WorkerRegistry`/`WorkerDef`-as-data home
  (Wave C6), the security-ordering gate, monotone de-escalation, and remote/MCP workers as future
  adapters.

- **C — Determinism / audit / governance (`fleet.*` + `WorkerLedger` + `fleet.replay`).** Strongest
  on the emphasized axis; the `WorkerLedger`-as-one-structure (tape + blackboard + record + resume)
  and `fleet.replay` as a CI gate are the most elegant ideas in the set; uniquely confronts replay
  fidelity under file mutation (path-leases, per-node snapshot pin). **Adopted as the audit core.**

- **D — Pragmatic-senior balance (`workflow.*` + typed-DSL-as-data, staged).** Best risk-adjusted
  staging (refactor → hidden-CLI engine → additive protocol), cleanest protocol hygiene
  (`session_id() = flow_id`, one event reusing existing types, spawn-as-gated-exec-tier), best
  invariant-by-invariant mapping. **Adopted as the spine.**

**The synthesis = D's staged spine + C's ledger/replay/path-lease audit core + B's
registry-as-data + A's FleetBudget/absence-at-floor.** Protocol bumps v3→v4 (additive variants are
exactly what a version bump means — a wording point A/C got slightly loose on).

---

## 12. Risks and open questions

**Risks (with mitigations):**

- **Replay fidelity under real file I/O** (the hardest problem). A CLI worker that mutates the tree
  changes the snapshot mid-run. *Mitigation:* pin `snapshot_generation` per node-start; treat file
  mutations as **recorded artifacts**, not live FS reads, during replay; **writer-node path-leases**
  forbid two writer-nodes racing the same paths. The byte-identical REPLAY CI gate (C4) catches any
  leak early.
- **Determinism erosion.** Any nondeterministic input (timestamps, completion order, model identity)
  leaking into a *scheduling* decision breaks golden tests. *Mitigation:* every scheduling decision is
  a pure function of `WorkflowDef` + the recorded ledger; **declared-order fold, never completion
  order**; `FakeWorker`/`ReplayWorker` golden tests as a CI gate.
- **DSL scope creep** into a Turing-complete VM. *Mitigation:* a **closed enum** of named combinators;
  no conditionals/loops in the data; arbitrary logic belongs to a (gated) worker. Additive-versioned
  like `RuntimeCommand`.
- **Cost / fork-bomb blowups.** *Mitigation:* `FleetBudget` (global `max_workers` + `remaining_usd`)
  self-cancels via `CancelToken`; default CLI workers non-recursive; `PolicyToolBox` gates each spawn;
  worst-case/fail-closed budgeting for unverified usage (gemini).
- **Approval fatigue / deadlock at fleet scale.** *Mitigation:* per-flow `ApprovalMode` +
  `DecisionMemory` (`AllowAlways`/`DenyAlways`); a per-node **deterministic** approval timeout
  (`APPROVAL_TIMEOUT`) that denies and records in the ledger; consider a per-flow "auto-approve
  read-only branches" default. No new approval mechanism.
- **Protocol over-commitment.** *Mitigation:* C0/C1 ship behind a hidden CLI with **no** protocol; C2
  lands only `Single`+`Parallel`; the `Strategy` enum is additive; resist adding fields until a
  concrete strategy needs them.
- **`gemini` path is UNVERIFIED** (honest note in `delegate_runtime.rs`). *Mitigation:* keep gemini
  one-shot, feature-flag it experimental, fail-closed on its budgeting until exercised live.

**Open questions:**

1. **Resume granularity.** Node-boundary resume is the v1 contract. Is sub-node (mid-turn) resume ever
   worth the checkpoint complexity, or is re-dispatch-from-last-instruction always sufficient?
2. **Pipeline + concurrent writers.** The path-lease serializes writer-nodes. Is there a real
   workflow that genuinely needs two nodes editing overlapping paths in parallel, or is "writers are
   serialized" an acceptable permanent constraint?
3. **`TaskTemplate` interpolation surface.** How much templating (string substitution from the ledger)
   is enough without becoming a logic language? Likely: named-output substitution only, no
   expressions.
4. **`FlowStore` artifact growth.** Full diffs/files in `ledger.jsonl` + `artifacts/` could grow
   unwieldy for long runs. When is the measured trigger to split artifacts out / move to SQLite?
5. **Cross-flow shared memory.** Does the long-term `FileMemoryStore` (`.nerve/memory.md`) become a
   read substrate for the fleet, or does each flow stay isolated to its own ledger?

When this document and the code disagree, treat it as a bug in one of them: either the change skipped a
seam (fix the change) or the seam genuinely evolved (update this doc in the same PR).
