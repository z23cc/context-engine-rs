# Agent Working Memory (checkpoint)

Status: **proposed** (design) — the "memory" capability flagged as the #1 agent gap.
Date: 2026-06-18
Related: `docs/designs/architecture-north-star.md` (seams, determinism boundary).

## 1. Problem

`nerve-agent`'s loop (`orchestrator.rs`) keeps the full turn history and, once
serialized history exceeds ~96k chars, **elides the oldest tool-output bodies**
(`HISTORY_COMPACT_THRESHOLD`, keep recent 8). There is **no working memory** —
nothing the agent maintains that (a) survives compaction and (b) carries the
current plan / decisions / next-steps across many turns. On long or complex
tasks the agent loses elided context and has no durable "current state." This is
the dominant agent-capability gap.

**Hard constraint: memory must not accumulate junk.** A naive append-everything
store (raw tool outputs, guesses, unexecuted plans, volatile state, easily
re-derivable facts) is worse than no memory — it poisons recall and burns
tokens. Anti-junk is a first-class design requirement, not an afterthought.

## 2. Goals / non-goals

**Goals**
- A **bounded, agent-curated** working-memory *checkpoint* (current plan,
  decisions, constraints, progress, next steps, key pointers).
- **Survives compaction**; re-pinned into every request, always current.
- Persisted with the session transcript; restored on resume (with a staleness mark).
- **Anti-junk by construction** (replace-not-append, capped, agent-only,
  forbidden-list, store-pointers-not-contents).
- **Minimal, seam-respecting**: exactly one additive `Hook` method in
  `nerve-agent`; everything else in the `nerve-workstation` composition root.

**Non-goals (deferred to a later design)**
- Long-term / cross-session distilled memory (the L1/L2/L3 / hindsight tier).
- Semantic compression of history (snapcompact-style).
- Vector/FTS recall.
- Auto-extraction of memory from tool outputs.

## 3. Borrowed principles (GenericAgent + oh-my-pi) — esp. anti-junk

| Principle | Source | Applied here |
|---|---|---|
| Working memory = **REPLACE not append**, bounded, agent-curated | GA `key_info` | the checkpoint is one evolving capped note |
| **Never auto-ingest tool output / thinking / bash** (the #1 junk) | oh-my-pi drops them pre-retention | checkpoint is *only* agent-written via the tool |
| **Only verified facts**; no guesses / unexecuted plans / unverified / volatile / logs | GA memory SOP | tool description forbidden-list |
| **"Re-derivable in a few steps → don't store"** | GA ROI test | "store pointers, not contents" |
| Recall order: stable context first, **volatile last** | oh-my-pi | checkpoint injected at the *end* of the request |
| Bounds / dedup / decay even for curated memory | oh-my-pi | hard size cap on write |

**nerve's unique leverage:** tools are deterministic + snapshot-backed + exactly
re-readable, so the "re-derivable → don't store" test almost always holds. Store a
`path:line` pointer + a conclusion, never the file body. nerve can keep the
thinnest memory of all — junk-minimal by construction.

## 4. Design

### 4.1 Data model (`nerve-workstation`)
```
struct Checkpoint { note: String }     // single bounded free-text notepad, REPLACE semantics
const CHECKPOINT_MAX_CHARS: usize = 1500;   // ~400 tokens; hard cap on write (truncate + marker)
```
Start with one `note` field (GA-style `key_info`) — simplest; structured fields
(`decisions`/`next_steps`/`pointers`) can come later if needed.

### 4.2 `update_checkpoint` tool (`nerve-workstation`, ToolBox seam)
Added to the agent's `ToolBox` via a decorator mirroring `SubAgentToolBox` /
`spawn_agent` (so `nerve-agent` is untouched). Args `{ note: string }`, REPLACE
semantics, capped. Description carries the anti-junk contract:

> Maintain your working memory: the current plan, decisions, constraints,
> progress, next steps, and key pointers (`path:line`). This **REPLACES** the
> previous note — keep it ≤ ~300 words. Store **pointers and conclusions, not file
> contents** (you can re-read exactly). **Do NOT store**: raw tool output,
> unverified guesses, unexecuted plans, ephemeral/volatile state, or anything you
> can reconstruct in a few tool calls.

Returns a tiny confirmation (never echoed back into memory).

### 4.3 Per-turn pinning seam (`nerve-agent`, additive — the only kernel change)
Extend the existing `Hook` trait with a per-turn, no-op-default method:
```
fn on_request(&self, _req: &mut ChatRequest) {}   // called in execute_turn, right before provider.chat
```
This is the **sole** `nerve-agent` change: additive, no-op default → a hook-free
run is byte-for-byte unchanged (consistent with the existing `Hook` contract).

### 4.4 `CheckpointHook` (`nerve-workstation`)
Holds `Arc<Mutex<Checkpoint>>` **shared with the `update_checkpoint` tool**. Its
`on_request` appends the current note to the request as a pinned block (a
`## Working memory` suffix on `req.system`). Because it is injected **fresh each
turn** (never stored in `history`), it (a) always reflects the latest note and
(b) **survives every compaction**. Placed last (after static system, after any
future long-term block) per the volatile-last recall order.

### 4.5 Compaction interaction
The checkpoint is **not** in `self.history`, so `compact_history` never touches
it; old tool outputs still elide as today. The checkpoint is the durable
"survives-everything" state — directly fixing the "forgets on long tasks" failure.

### 4.6 Persistence + resume (`nerve-workstation`)
The checkpoint is saved with the `SessionRecord` (`session.rs`) at each turn
finish. On `session.start { resume }` / agent resume it is restored into the
shared store with a staleness marker appended (GA-style):
`[restored from a prior session — update or clear it if the task has changed]`.

### 4.7 Anti-junk enforcement (recap)
1. REPLACE semantics → bounded, cannot grow.
2. Hard char cap on write (truncate + `[truncated]` marker).
3. Agent-curated only — **tool outputs never auto-flow into the checkpoint**.
4. Tool-description forbidden-list (the GA "don't store").
5. Store pointers, not contents (determinism leverage).

## 5. Architecture fit (north-star)
- **Determinism boundary intact**: checkpoint data + tool live in
  `nerve-workstation`; the only `nerve-agent` change is an additive `Hook`
  method (agent layer). `nerve-core` untouched.
- **Seam-respecting**: tool enters via the `ToolBox` seam (like `spawn_agent`);
  per-turn injection via the `Hook` seam (extended additively). Wiring of the
  shared store happens only in the binary (`run_agent` / `session_manager`).
- **No protocol change**: the checkpoint is internal agent state; `update_checkpoint`
  is just another ToolBox tool. (A future `session` event could surface it.)

## 6. Crate placement
- `nerve-agent`: `Hook::on_request` (additive) + orchestrator calls it per turn.
- `nerve-workstation`: `Checkpoint`, `update_checkpoint` tool (ToolBox decorator),
  `CheckpointHook`, wiring in `run_agent`/`session_manager`, persistence in `session.rs`.

## 7. Turn flow (with checkpoint)
```
build request from history
  → CheckpointHook.on_request: append current note to req.system   ← survives compaction, always current
  → provider.chat (stream)
  → dispatch tool_calls (the model may call update_checkpoint → REPLACE the note)
  → compact_history (elides old tool outputs; never the checkpoint)
finish turn → persist checkpoint with the transcript
```

## 8. Testing
- `update_checkpoint`: REPLACE + cap (truncate marker) — workstation unit.
- `CheckpointHook.on_request` injects the note into the request — unit.
- **Pin survives compaction**: orchestrator-level test (hook + `MockProvider`)
  asserting the note appears in *every* request even after history elision.
- Persistence + resume restores the note with the staleness marker.
- `Hook::on_request` default no-op leaves hook-free runs unchanged (existing tests pass).

## 9. Phasing
- **This doc = MVP** (working memory only).
- Deferred (separate design): long-term distilled memory (verified-only, bounded,
  recall block, recalled-content stripped before re-store) + semantic compaction.

## 10. Open decisions
1. Over-cap write → **truncate + marker** (recommended, like `cap_tool_output`) vs reject.
2. Checkpoint shape → single `note` (recommended for MVP) vs structured fields.
3. Injection site → `req.system` suffix (recommended) vs trailing message.
4. Soft junk-guard rejecting raw-output-looking blobs → **defer** (rely on prompt + cap).

## 11. References (file:line)
- **GenericAgent** — `key_info` REPLACE (`ga.py:439`), schema "Don't store:
  ephemeral/obvious/old-task" (`tools_schema.json:57`), `_get_anchor_prompt`
  re-injection (`ga.py:537`), verified-only long-term rules + "discard
  reconstructable/common-knowledge" (`memory/memory_management_sop.md`), ROI
  cleanup (`memory/memory_cleanup_sop.md`).
- **oh-my-pi** — drop tool/thinking/bash before retention (`hindsight/transcript.ts:20`),
  strip recalled memory before re-store (`hindsight/content.ts:25`), bounds/TTL/dedup
  (`mnemopi/.../store.ts`), "capture only if reusable, else do nothing"
  (`autolearn-nudge.md`), recall order static→stable→volatile (`hindsight/backend.ts:95`).
- **nerve** — loop + compaction (`crates/nerve-agent/src/orchestrator.rs`),
  `Hook` trait (same file), tool-injection template (`crates/nerve-workstation/src/subagent.rs`),
  persistence (`crates/nerve-workstation/src/session.rs`).
