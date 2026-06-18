# Agent Long-Term Memory (cross-session, distilled)

Status: **proposed** (design). Follows `agent-working-memory.md` (the within-session tier).
Date: 2026-06-18
Related: `docs/designs/architecture-north-star.md`, `docs/designs/agent-working-memory.md`.

## 1. Problem

The working-memory checkpoint (shipped v0.0.39) is **within-session** — it dies when
the run ends. nerve still does not **learn across sessions**: every `nerve agent run`
starts blank, re-discovering the same user preferences, project conventions, and
hard-won fixes. We want a durable, cross-session memory of **distilled, verified**
facts — recalled automatically at session start.

**The same hard constraint, but sharper:** long-term memory persists *forever*, so junk
here compounds across every future session. Curation must be stricter than for working
memory.

## 2. Goals / non-goals

**Goals**
- A project-local, cross-session store of durable facts (preferences, conventions, gotchas).
- **Recalled automatically** into the system prompt at session start.
- **Agent-curated, verified-only, bounded**; stricter anti-junk than working memory.
- **Reuses existing seams** — ideally zero `nerve-agent` change.

**Non-goals (deferred)**
- Vector/semantic recall, SQLite, LLM auto-extraction (see §3 decision).
- Multi-tier decay/consolidation, cross-project/global memory.
- Automatic end-of-session distillation (start with an agent-invoked tool).

## 3. Decision: a `MemoryStore` port, file-first, promoted on evidence

The real question is **not file-XOR-SQLite** — that is a *backend* choice. The decision is:
**(a)** put storage behind a **`MemoryStore` port** (matching nerve's `CatalogProvider` /
`LlmProvider` culture, so the backend is never a one-way door); **(b)** decompose "memory"
into three subsystems and give each the right store; **(c)** ship the **file** backend for
the durable-facts subsystem now; **(d)** promote to SQLite only on a *measured* trigger.

The references diverge — GenericAgent = markdown files + agent-distillation; oh-my-pi =
SQLite + LLM fact-extraction + vector/FTS recall. We take GA's shape for subsystem ① and
defer oh-my-pi's machinery, but **behind the port** so either can back it later.

### 3.1 Three subsystems, three stores (do not conflate)
| Subsystem | Nature | Store |
|---|---|---|
| ① Distilled facts (this doc) — preferences / conventions / gotchas | small, curated, **always injected whole** | **file** (`FileMemoryStore`); SQLite overkill |
| ② Episodic / session history — what happened | large, append, occasionally queried | JSON files today; **SQLite** when query needs arise (P5) — a *separate* decision |
| ③ Semantic recall — find by meaning | needed only at scale | **reuse the existing `semantic` engine**; never a 2nd vector stack |

Conflating these into one SQLite DB — to serve ① now on the *speculation* that ②/③ will
need it — is itself an over-coupling that violates the north-star (enter via a seam,
promote on evidence, don't pre-build).

### 3.2 Why file-first for ①
1. **Determinism boundary.** Keep the agent layer free of a DB / embedding dep. nerve's
   dense embeddings are a **separate opt-in `semantic` core feature**; entangling memory
   with it pulls a ~300 MB model into every run. SQLite would be nerve's **first DB
   dependency** + a schema-migration burden for the project's whole life.
2. **Reuse proven seams, zero `nerve-agent` change.** Recall = the existing
   `Hook::on_start`; write = a `ToolBox` decorator (the shipped `CheckpointToolBox`
   pattern); persistence = `.nerve/` (transcripts already live there as JSON).
3. **Determinism leverage.** Because the tools re-derive almost anything exactly, ① holds
   only the *non-reconstructable residue* (preferences, decisions+rationale, non-obvious
   conventions, gotchas) — never file locations / code structure. Small → always-injectable,
   naturally bounded, a poor fit for a vector DB.
4. **Transparency is a workstation asset.** A user can `cat` / edit / `git`-version
   `.nerve/memory.md` — they *see and correct* what their agent knows. An opaque DB forfeits
   that, and it aligns with nerve's deterministic / inspectable identity.

### 3.3 Promote ① to a `SqliteMemoryStore` when (any, measured)
- per-project facts exceed the always-inject token budget (→ need selective recall);
- concurrent multi-agent `remember` writes cause real lock contention / lost updates;
- structured queries appear (by project / tag / recency / provenance / contradiction).
Behind the port this is a backend addition, not a redesign.

### 3.4 Honest counter-case
SQLite-from-start is defensible **if** the team values one uniform store over transparency
**and** subsystem ② is large-and-queryable from day one. The cost: the first DB dep, a
lifelong schema-migration surface, lost transparency — and it does **not** solve the actual
hard problem (recall *quality* / curation). "Super" comes from memory quality + right-time
recall, not the backend engine; optimize that axis first.

## 4. Borrowed principles + anti-junk (stricter than working memory)

| Principle | Source | Applied |
|---|---|---|
| Only **verified** facts (from successful tool results) | GA hard rule | tool desc + write gate |
| **Durable / stable across sessions** only | oh-my-pi seeds | exclude transient task state (that's working memory) |
| **"Re-derivable in a few tool calls → don't store"** — a *high* bar here | GA ROI | store only the non-reconstructable residue |
| **Strip recalled memory before re-storing** | oh-my-pi | don't re-remember what's already present |
| **Bounded + prune lowest-value** | GA L1 caps / ROI cleanup | hard cap; over-cap forces prune |
| Recall order: stable memory **before** volatile working memory | oh-my-pi | on_start injects L1, on_request injects the checkpoint |

## 5. Design

### 5.1 Storage — the `MemoryStore` port (`nerve-workstation`)
Storage sits behind a `MemoryStore` trait (load / append-deduped / cap). The MVP impl is
`FileMemoryStore` over `.nerve/memory.md` per workspace (sibling of the session
transcripts) — plain markdown, human-readable. One layer for the MVP:
- **L1 (always-injected)**: a bounded list of durable facts. Cap `LONG_TERM_MAX_CHARS`
  (~2000 chars / ~30 bullet lines). (An on-demand **L2** detail file is a future extension;
  not in the MVP.)
- A future `SqliteMemoryStore` implements the same trait — the tool + recall hook are
  unchanged (see §3.3 triggers).

### 5.2 Recall — `Hook::on_start` (EXISTING seam, no nerve-agent change)
A `MemoryHook` (workstation) whose `on_start` reads `.nerve/memory.md`, and if non-empty
appends a `## Project memory (durable facts learned in past sessions)\n{L1}` block to the
system prompt — **before** the working-memory checkpoint (stable-before-volatile order).
Empty file → injects nothing → zero cost until the agent learns something.

### 5.3 Write — `remember` tool (`ToolBox` decorator, the CheckpointToolBox pattern)
`remember { fact: string }`: appends a verified, durable fact **via the `MemoryStore`**
(the `FileMemoryStore` persists to `.nerve/memory.md`). Dedups against existing entries;
over-cap → returns the current list and asks the agent to prune/replace rather than
silently dropping. Description carries the strict
anti-junk contract:

> Record a **durable, verified** fact worth remembering in future sessions — a user
> preference, a non-obvious project convention, or a hard-won fix/gotcha. Only record what
> a **successful** action verified. Do **NOT** record: file locations or code structure (a
> tool re-finds those exactly), transient task state (use update_checkpoint), unverified
> guesses, unexecuted plans, or anything reconstructable in a few tool calls. Keep each fact
> one tight line.

### 5.4 Within-session vs cross-session (clean split)
- **Working memory** (checkpoint): current task, pinned every turn, dies at run end.
- **Long-term memory** (this): durable facts, injected once at session start, persists.
- A mid-session `remember` writes to the file; it is recalled on the **next** session's
  `on_start` (current-session currency is the checkpoint's job). No shared `Arc` needed —
  the file is the medium.

## 6. Architecture fit (north-star)
- **Zero `nerve-agent` change** (recall uses the existing `Hook::on_start`; write uses the
  `ToolBox` seam). `nerve-core` / `nerve-runtime` / protocol untouched.
- Composition (registering `MemoryHook` + the `remember` decorator, resolving the
  `.nerve/memory.md` path) only in the binary — same place the checkpoint was wired
  (`subagent.rs` shared assembly + `run_agent`).
- Determinism boundary intact: memory is non-deterministic agent state, file-backed in the
  workstation, never in the kernel.

## 7. Crate placement
- `nerve-workstation`: a `memory` module — the `MemoryStore` trait (port), the
  `FileMemoryStore` impl (load/append-deduped/cap over `.nerve/memory.md`), the `remember`
  ToolBox decorator, and the `MemoryHook` impl of `on_start`; path resolution + wiring in
  `subagent.rs` / `agent.rs`. Split into a `memory/` dir if it exceeds the 600-line gate.
- No other crate changes (recall reuses the existing `Hook::on_start`).

## 8. Flow
```
session start → MemoryHook.on_start: read .nerve/memory.md → inject L1 into system prompt
… agent works; on a durable verified learning → calls remember{fact} → dedup+append (cap-guarded)
next session → on_start recalls the now-larger memory
```

## 9. Testing
- `remember`: append, dedup (no duplicate line), over-cap returns prune request (workstation unit).
- `MemoryHook.on_start`: empty file injects nothing; non-empty appends the block before the
  checkpoint block; bounded to the cap.
- Round-trip: remember in one run → present in the next run's injected system prompt
  (integration, using a temp `.nerve`).
- Path isolation: writes land under the workspace `.nerve/`, not global.

## 10. Phasing
- **This doc = L1 MVP** (file, agent-invoked remember, on_start recall).
- Deferred: on-demand **L2** detail file; **auto-distillation** (opt-in — full design in §13);
  **ROI/decay pruning**; only if needed — SQLite + vector recall.

## 11. Open decisions
1. Tool name: `remember` (recommended) vs `update_long_term_memory`.
2. Over-cap policy: **return current list + ask agent to prune** (recommended) vs auto-drop oldest.
3. Should `.nerve/memory.md` be git-committable (team-shared conventions) or always local?
   (Recommend: local by default — it's under the already-gitignored `.nerve/`; a future
   `--shared-memory` could point at a committed path.)
4. Auto-distillation — designed in §13 as an **opt-in** post-loop step (not a hook; `on_end`
   cannot call the LLM). Off by default; agent-invoked `remember` is the zero-cost default.

## 12. References (file:line)
- **GenericAgent** — `start_long_term_update` distillation + verified-only / "discard
  reconstructable" rules (`memory/memory_management_sop.md`), L1 caps (`ga.py`), global-memory
  injection into the system prompt (`agentmain.py:39`), ROI cleanup (`memory/memory_cleanup_sop.md`).
- **oh-my-pi** — mnemopi SQLite + fact extraction (deferred), hindsight "capture only durable,
  else nothing" (`autolearn-nudge.md`), recall order stable→volatile (`hindsight/backend.ts:95`).
- **nerve** — existing `Hook::on_start` / `EnvironmentHook` (recall seam), `CheckpointToolBox`
  (write-decorator pattern, `crates/nerve-workstation/src/checkpoint.rs`), `.nerve/` persistence
  (`crates/nerve-workstation/src/session.rs`).

## 13. Auto-distillation (opt-in) — design

Agent-invoked `remember` (shipped v0.0.41) populates memory only when the agent notices a
durable fact mid-task — unreliable. Auto-distillation reliably extracts durable facts after
a session. But it is the **junk hotspot** (an LLM auto-extracting over-records) and costs one
extra LLM call per session — so it is **opt-in, off by default**, behind layered gates.

### 13.1 Where it runs (zero `nerve-agent` change)
Not a hook — `on_end` only observes `RunOutcome` and cannot make LLM calls. It is a post-loop
step in the composition root (`subagent.rs::run_at_depth`, after `run_collecting`), reusing
the run's provider + the `remember` tool as a **second, restricted `Orchestrator` run**.
`AgentRunOutput { outcome, history }` already provides the finished `history` (review context)
and `outcome.turns` (the cost gate), so no plumbing change is needed.

### 13.2 The distiller
- Provider: the run's provider.
- Toolbox: **`remember` only** (`MemoryToolBox` over a no-op inner) — it can ONLY write memory.
- Context: `with_history(output.history)` (cap/compact if large) + the **current memory**
  (so it won't re-record — strip-recalled-before-store).
- AgentDef: the strict distiller prompt below, `max_turns: 2`.

### 13.3 The strict prompt (anti-junk core)
> Review the session above. Your ONLY job: if it produced a DURABLE, VERIFIED fact worth
> recalling in FUTURE sessions — a user preference, a non-obvious project convention, or a
> hard-won fix/gotcha — call `remember` once per such fact. Apply a HIGH bar: most sessions
> produce nothing worth keeping. Do NOT record anything a tool can re-derive (file locations,
> code structure, APIs), transient task state, unverified guesses, unexecuted plans, or
> anything already in project memory (shown below). If nothing qualifies, do nothing and stop.
> When unsure, do not record.

(GA "ignore if nothing verified"; oh-my-pi "capture only if reusable, else do nothing".)

### 13.4 Cost gates
- **Opt-in**: `--distill-memory` (CLI) / config; OFF by default.
- **Top-level only** (`depth == 0`) — never subagents (would be N extra calls).
- **Substantive + completed**: `outcome.turns >= DISTILL_MIN_TURNS` and the run finished
  normally (not cancelled/errored). Trivial/failed runs distil nothing.

### 13.5 Layered anti-junk (defense in depth — this is the hotspot)
1. Opt-in (off by default). 2. `remember`-only toolbox. 3. High-bar prompt ("most → nothing").
4. `remember`'s existing guards: dedup, 2000-char cap, forbidden-list, OverCap→prune.
5. Pass current memory in → no re-records. 6. `max_turns: 2`.

### 13.6 Testing
- Mock provider calls `remember` for a durable fact → store gains it.
- Mock provider returns Stop with no tool calls → memory unchanged ("nothing to keep").
- Gates: `turns < DISTILL_MIN_TURNS` → never runs; `depth > 0` → never runs; flag off → never runs.

### 13.7 Open decisions
- `DISTILL_MIN_TURNS` value (or gate on edits-made rather than turn count).
- Full vs compacted history into the distiller (cost vs recall fidelity).
- Flag / config surface name.

Build-ready; stays a separate opt-in phase until green-lit.
