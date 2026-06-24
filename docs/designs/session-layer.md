# Session Layer — interactive multi-turn agent (design)

Status: in progress. The "keystone" beyond P0's one-shot `agent.run` — see
`architecture-north-star.md` (the interactive Session layer the roadmap names as the
prerequisite for a conversational GUI, approvals, and resume).

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc specs the durable, resumable session/job store that is now a precondition (the credibility floor) for the substrate — extend it to the delegate seam and the Run/Ledger so state survives restart and is enumerable/replayable.

## Goal

Turn one-shot `agent.run` into interactive, multi-turn **sessions**: send a message →
the agent works (streaming) → send a follow-up on the same conversation; steer/interrupt
mid-run; **approve gated tools over the protocol**; **resume** a past session. This unlocks a
real conversational GUI and human-in-the-loop tool use.

`agent.run` stays (the simple one-shot path). Sessions are an additive layer on top.

## Invariants (unchanged — see north-star §3)

- Protocol vocabulary lives in **nerve-runtime as DATA** (additive, versioned); nerve-runtime
  **never** depends on nerve-agent. Provider/model/text/decision are plain strings/JSON.
- Execution/composition lives in the **workstation** (the session manager). The nerve-agent
  change is **minimal and additive**.
- Determinism boundary intact; all tools go through `Runtime`.

## Protocol (nerve-runtime, additive — codegen to TS after any change)

Session command family (data; match the existing `RuntimeCommand` tag style):
- `session.start { workspace?, provider, model, system_prompt?, agent?, resume? (session_id), max_turns?, temperature?, reasoning_effort?, tool_filter? }` → `{ session_id }`
- `session.message { session_id, text }`
- `session.interrupt { session_id }`
- `session.respond { session_id, request_id, decision: "allow" | "deny" }`  ← approval reply
- `session.get { session_id }` / `session.list` / `session.close { session_id }`

Events (session-scoped; carry `session_id`):
- session lifecycle: `session_started` / `turn_started` / `session_idle` / `session_closed` (keep minimal)
- **reuse** the existing `AgentEventKind` payloads (assistant text / reasoning / tool started/finished), tagged with `session_id`
- `approval_requested { session_id, request_id, tool, arguments }`

`handle_command` in core runtime returns a clear "executed by the host session manager" error for
session commands (like `agent.run`); the **workstation intercepts** them. Add to
`RUNTIME_COMMAND_NAMES`. Run `cargo run -p nerve-runtime --bin export-runtime-protocol`; the drift
gate (`export-runtime-protocol -- --check`, run by `cargo test -p nerve-runtime`) must pass.

## Backend (workstation session manager) + minimal nerve-agent change

- **SessionManager**: live sessions `id → { history: Vec<Message>, provider/model/def, status, store handle }`.
- **Multi-turn = seed-history.** Each `session.message` runs the `Orchestrator` seeded with the
  session's accumulated history + the new user message; resulting messages append to history and
  persist (reuse P5 `session.rs`). *nerve-agent change (minimal, additive):* the `Orchestrator`
  accepts an initial conversation history (e.g. `with_history(Vec<Message>)` / a `run` that takes
  prior messages) so a session continues across messages. No new orchestrator concepts.
- **Approval over protocol.** A `ProtocolApprover` implementing P4's `Approver`: on `Ask`, emit
  `approval_requested{request_id}` and **block on a per-session channel** (`recv_timeout`);
  `session.respond` pushes the decision into that channel → allow/deny. Replaces the daemon's
  auto-deny *for sessions*. Sync, fits nerve (the tool call runs on a job thread).
- **Interrupt.** `session.interrupt` cancels the current turn's `CancelToken`.
- **Resume.** `session.start { resume }` loads the P5 transcript → reconstructs history → continues.
- The session manager maps orchestrator `AgentEvent`s → `RuntimeEvent` tagged with `session_id`.
- Wire `session.*` through the daemon (stdio + HTTP/SSE) the same way `agent.run` jobs are wired.

## Surfaces

- Daemon: `session.*` over stdio + HTTP/SSE (S2).
- Optional CLI: `nerve agent chat` (local interactive multi-turn) — nice-to-have.
- GUI: upgrade `daemon/gui.html` to a multi-turn chat with approval prompts (S3).

> Interactive-CLI convergence (the right end-state for `nerve agent run` / `nerve agent chat`): make
> the CLI a **true transport client** of `session.*` — render the event stream, approve gated tools
> via the `session.respond` round-trip — **not** an in-process `RuntimeCommand` round-trip. Today the
> CLI is a *sanctioned local* client of the shared `agent::run_agent` executor (north-star §2); the
> protocol-client form is exactly this Session layer, so the convergence lands here, not as bespoke
> CLI→hub command routing.

## Phasing

- **S1** — protocol vocabulary (data) + codegen.
- **S2** — session manager + orchestrator seed-history + approval channel + interrupt + resume + daemon wiring.
- **S3** — GUI multi-turn chat + approval UI (session.start / message / respond / interrupt).
