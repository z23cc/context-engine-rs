# Sub-agents — an agent spawns sub-agents (design)

Status: in progress. Multi-agent orchestration: a running agent can delegate a
subtask to a fresh sub-agent and continue with its result.

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc describes how sub-agent roles feed candidate generation; a read-only reviewer/oracle/scout sub-agent is purely advisory (INV-R3) and never produces an authoritative correctness verdict.

## Goal

Let an agent delegate a subtask to a **sub-agent** (its own conversation /
provider / model / tools), get the result back, and continue — the classic
"Task / subagent" pattern. The parent reasons over the sub-agent's answer.

## Approach: `spawn_agent` is just another TOOL, injected at the composition root

- The agent gains a `spawn_agent` tool. Calling it runs a **nested orchestrator**
  (`run_agent`) and returns the sub-agent's final result as the tool result.
- It is injected via the **ToolBox seam** in the workstation (composition root) —
  a decorator that adds `spawn_agent` to the agent's toolbox. **nerve-agent
  (orchestrator) and nerve-runtime (protocol) are UNCHANGED**: from the
  orchestrator's view it is one more tool in `toolbox.specs()`; no new protocol
  types, no new entry point.

## Invariants (unchanged — north-star §3)

- Determinism boundary: sub-agents run **above** the core, calling tools through `Runtime`.
- Seam discipline: `spawn_agent` is added at the composition root via the existing
  `ToolBox` seam; nerve-agent / nerve-runtime untouched; the agent's `tool_filter`
  and the P4 policy can gate it like any tool.

## Mechanics

- Tool: `spawn_agent { task, agent?, provider?, model? }` → result
  `{ final_text, turns, usage }` (a concise summary the parent reads).
- Recursion via one `run_agent_at_depth(depth, …)`: builds `RuntimeToolBox` →
  `ToolGate(policy)` → **if `depth < max_depth`**, wrap with a `SubAgentToolBox`
  that adds `spawn_agent`; resolve the provider (inherit the parent's
  provider/model unless overridden; resolve via `ProviderRegistry`; an `agent`
  name resolves a P3 `AgentDef`); run the orchestrator. `spawn_agent`'s handler
  calls `run_agent_at_depth(depth + 1, sub_config, cancel, sub_sink)`.
  Encapsulate as a `SubAgentSpawner { runtime, registry, policy, max_depth }`
  shared by `Arc` so the recursion is clean.
- Bounds: `max_depth` (default 2) — at max depth the sub-agent's toolbox omits
  `spawn_agent` (a depth-exceeded call errors). Prevents runaway recursion.
- Cancellation: the parent `CancelToken` propagates into sub-agents (interrupt
  stops the whole tree).
- Policy: sub-agents inherit the parent's `ToolGate`/policy (their mutating tools
  are gated too).
- Events (MVP): the sub-agent runs with an internal sink; the parent already
  emits `tool_started{spawn_agent,…}` / `tool_finished{output = final_text}`, so
  the user sees the spawn + result. Surfacing nested live sub-agent events to the
  GUI is a follow-up (no protocol change needed for MVP).

## Surfaces

- Wired into `run_agent`, so available to both `agent.run` (CLI/one-shot) and
  session turns. An agent def (P3) can include/exclude `spawn_agent` via `tool_filter`.

## Phasing

- **A1** — backend: the `spawn_agent` tool + `SubAgentToolBox` + recursive
  `SubAgentSpawner`/`run_agent_at_depth` + depth bound + cancellation propagation
  + policy inheritance + tests.
- *(optional A2 — surface nested sub-agent events to the GUI.)*
