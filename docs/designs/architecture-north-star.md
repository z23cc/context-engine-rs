# Architecture North Star

Status: **governing** — read before any structural change. Referenced as a binding rule from `CLAUDE.md`.
Date: 2026-06-18 · reconciled to implementation 2026-06-19 · **product direction updated 2026-06-23**

This is the long-term architectural contract for Nerve Workstation. It exists so that every
incremental change is **locally optimal _and_ globally aligned**: each feature plugs into a declared
seam instead of bolting on a new bespoke entry point. When in doubt, the seam wins over the shortcut.

## 1. North star

> **Nerve = a deterministic code-intelligence kernel (exposed as tools) + a protocol-defined runtime
> that is a _cockpit for orchestrating multiple external CLI coding agents_ (Claude Code, Codex,
> Gemini CLI, …) side by side — "everything else is a plugin behind a port."**

**Product identity (set 2026-06-23).** Nerve's job is to **manage and observe external agent CLIs,
not to be one.** The deterministic engine is offered *to* those agents (as MCP tools); the runtime
drives them through the `delegate.*` seam, surfaces their tool calls / approvals / output in one
cockpit, and lets you run several agents across projects concurrently. The built-in `nerve-agent`
LLM loop (the `agent.run` / `session.*` own-engine path) is **demoted to a secondary, optional
seam** — retained for headless/embedded use, but no longer the primary product surface and not
featured in the GUI. This **reverses** the earlier "runtime session as the primary cockpit" framing;
see the dated note in §2, the scorecard in §5, and the roadmap in §8.

- The **kernel** (`nerve-core`) is pure and deterministic; golden-tested; never extended by runtime
  plugins. Its tools are the value Nerve hands to the agents it orchestrates.
- Non-determinism (the external agent CLIs, LLMs, third-party tools, sessions, network, time) lives
  strictly **above** the kernel.
- Extending the system means **implementing a declared seam** — never editing the kernel and never
  opening a new ad-hoc host entry point. Supporting a new agent CLI = the **`delegate` seam**, not a
  new face.

## 2. Prime directive (local-optimal = global-optimal)

> **Every new capability MUST enter through a declared seam (port / registry / protocol). Never open
> a bespoke entry point.**

Cautionary tale (resolved as designed — reconciled 2026-06-19): `nerve agent run` was *originally* a
synchronous CLI path that bypassed `RuntimeCommand` / job / event — a third, off-protocol "face" that
broke the protocol-authority invariant (§3.3). P0 folded the agent into the protocol: `agent.run` is
now first-class vocabulary (`RuntimeCommand::AgentRun`), the daemon executes it as a cancellable job
emitting structured agent events, and **both** the daemon and the CLI converge on the single host
executor `agent::run_agent` — so all tool execution in both faces flows through `Runtime` via
`RuntimeToolBox` (§3.2 holds). The CLI is therefore a **sanctioned local, synchronous, interactive
client** of that shared executor, not an off-protocol face. It deliberately does *not* round-trip a
`RuntimeCommand`: the protocol type is a **lossy projection** of `AgentRunConfig` — it must never
carry `api_key` (§3.4 transport-neutral; §3.7 broker topology) and does not carry the CLI-only
`distill_memory` / `verify_completion` opt-ins — so building the config directly from CLI flags is
correct, not debt. The lesson stands — **a capability without a seam is guaranteed debt** — but this
capability now has its seam (`RuntimeCommand::AgentRun` + the shared `run_agent` executor). The
remaining convergence — turning the interactive CLI into a true *transport* client with protocol
approval round-trips (`session.respond`) — is Session-layer / P6 work, not in-process command routing.

**Direction note (2026-06-23).** The two agent-execution faces are now ranked. The **`delegate.*`
seam** — drive an external CLI (codex / claude / gemini) as a sandboxed, steerable subprocess — is
the **primary, product-facing** path; the **own-engine** face (`agent.run` / `session.*` over
`nerve-agent`) is **secondary/optional**. Both remain proper protocol vocabulary and both flow through
`Runtime` (§3.2 holds), so this is a priority/surfacing decision, **not** an invariant change. The
cockpit's differentiator is orchestrating *many* such CLIs at once (§8), not a better single loop.

## 3. Invariants (do not break)

1. **Determinism boundary.** `nerve-core` is deterministic and golden-tested. Nothing
   non-deterministic (LLM calls, network, wall-clock, third-party plugins) may enter `nerve-core`;
   it all lives in `nerve-runtime` / `nerve-agent` / `nerve-workstation`. Golden snapshots
   (`crates/nerve-core/tests/snapshots/*.snap`) guard this.
2. **Runtime is the single dispatch hub.** All tool execution goes through `Runtime`
   (`handle_tool_call*` / `handle_command*`). No host (MCP, daemon, agent, CLI) may call
   `nerve-core` dispatch directly.
3. **Runtime types are the single protocol authority.** The human-facing runtime protocol vocabulary
   (`RuntimeCommand`, `RuntimeEvent`, `Runtime*Request`, method-name constants) is defined **only** in
   `nerve-runtime`, exported to `docs/protocol/*` via `cargo run -p nerve-runtime --bin
   export-runtime-protocol`, and drift-checked in CI (the `export-runtime-protocol -- --check` gate +
   the `generated_protocol_rust_artifacts_are_current` test). Protocol changes are **additive and
   versioned** — never break a published field.
4. **Protocol types are transport-neutral data.** Commands/events carry plain serde/JSON
   (e.g. `tool.call` = `{name, arguments: Value}`), never references to engine/agent domain types.
   Consequently `nerve-runtime` depends **only** on `nerve-core` — never on `nerve-agent`. The
   composition root translates protocol data ⇄ domain types.
5. **MCP is a separate, external protocol.** The MCP face (`server.rs`, pinned `2024-11-05`) consumes
   the `Runtime` dispatch hub but owns its own wire vocabulary (the MCP standard). It is **not** under
   the runtime-protocol authority. Do not conflate the two; never put session/agent vocabulary into
   the MCP face.
6. **Composition only in the binary.** `nerve-workstation` is the sole composition root (wires
   adapters, toolbox, session manager, persistence). Sibling crates `nerve-runtime` and `nerve-agent`
   never depend on each other.
7. **OAuth login topology — callback capture is the client's job, never the daemon's.** Providers
   allowlist **localhost** redirect URIs (OpenAI `:1455`, xAI `:56121` fixed; Anthropic `:54545`), so
   the OAuth redirect only ever lands on the machine running the browser. Login is therefore staged
   and the daemon stays **stateless**: `auth.start` with `flow=browser` returns the authorize URL + a pending id; the
   client opens the browser, captures the `?code=` redirect, and calls `auth.complete`. `flow=device_code`
   is protocol vocabulary for mobile/remote clients but currently fails closed until provider device endpoints are wired. The daemon
   **must not own a keep-alive loopback** — for a remote daemon (Tailscale/mobile) the redirect lands
   on the client, not the daemon, so a daemon loopback structurally cannot catch it and adds nothing
   over client capture. Token holding/refresh is the composition-root "broker" role (`AuthManager` +
   `nerve_agent::auth::ensure_fresh`, single-flight). Corollaries: a daemon-served **web page cannot
   bind a socket**, so the web GUI keeps the paste fallback while native clients (Tauri/mobile)
   capture programmatically; mobile/remote zero-paste is solved by a **token-sharing broker** (log in
   once on a trusted node; the refresh token never leaves it), with device-code flow as the secondary
   fallback — **not** by a loopback. "Paste the code" is only the degenerate manual fallback.
8. **Agent memory enters through a `MemoryStore` port — file-first, promoted on evidence.**
   Memory is non-deterministic agent state: it lives in `nerve-workstation`, never in
   `nerve-core`. It is **three subsystems behind one port**, not one store — durable
   distilled facts (small, always-injected → `FileMemoryStore` over `.nerve/memory.md`),
   episodic / session history (large, queryable → P5 persistence; SQLite when needed), and
   semantic recall (**reuse the `semantic` core feature**, never a second vector stack).
   Write enters via the `ToolBox` seam (`remember`), recall via the `Hook::on_start` seam
   (zero `nerve-agent` change). Promote a backend (file → SQLite) only on a *measured*
   trigger — always-inject token budget exceeded, real write contention, or a structured-
   query need — never speculatively. See `docs/designs/agent-long-term-memory.md`.
9. **The permission gate is the outermost toolbox boundary.** P4 authorization
   (`PolicyToolBox`) must wrap the *entire* tool decorator stack, so every tool the model can
   call — read tools, `spawn_agent`, decorator-added tools (`update_checkpoint`, `remember`),
   and any future `run_command` — passes through it. Safe tools are classified **Allow** in
   the policy, never left ungated by sitting outside the gate; write/exec tools are **Ask**.
   **Containment is separate and orthogonal:** P4 decides *whether* a call runs; the
   `SandboxLauncher` port decides *what the spawned process may touch*. Execution capability
   is bound to the **trust context** (local CLI may use the best-effort launcher; a
   daemon/remote run must require a strong-isolation backend or refuse). See
   `docs/designs/agent-exec-sandbox.md`.

## 4. Crates & layers (current)

```
nerve-core       deterministic kernel — CatalogProvider port → immutable CatalogSnapshot;
  ▲   ▲          tools (search/read/tree/codemap/repomap/navigate/edit/semantic/build_context);
  │   │          dispatch hub entry (handle_tool_call*). Golden-tested. Depends on nothing internal.
  │   │
  │   └── nerve-agent   LLM layer — LlmProvider trait + Anthropic/OpenAI-Responses/xAI adapters,
  │                     multi-provider OAuth + credential store, Orchestrator loop, ToolBox port.
  │                     Depends only on nerve-core (for CancelToken). Runtime/protocol-agnostic.
  └────── nerve-runtime  protocol authority + dispatch hub wrapper + RuntimeToolAdapter registry +
                         job/event protocol (v3). Depends only on nerve-core.
                              ▲
nerve-workstation   composition root (the `nerve` binary): MCP face (server.rs), daemon face
  ▲                 (daemon/, jobs.rs), CLI (cli.rs), agent wiring (agent.rs = RuntimeToolBox),
  │                 xAI tools (xai/), thin `nerve auth` alias (auth/ → nerve-agent::auth).
  │
nerve-tui (+ future GUI/mobile)   clients of the versioned runtime protocol — never the engine.
```

`nerve-agent` and `nerve-runtime` are **siblings** (both depend only on `nerve-core`); the binary
marries them via the `ToolBox` port (`RuntimeToolBox` in `agent.rs`).

## 5. Seam scorecard

Most plugin seams now **exist *and* are wired** (✅ below); the residual work is promoting a few to
first-class registry/config APIs and building the orthogonal containment half of P4. (Reconciled to
code 2026-06-19 — the layers the original draft marked ✗ have since landed.)

| Seam (port) | Defined in | Today | Remaining work |
|---|---|---|---|
| `CatalogProvider` | `nerve-core/port.rs` | Fs / Memory | compile-time; fine (could add Git / remote overlay) |
| `RuntimeToolAdapter` | `nerve-runtime` | ✅ xAI (first-party) **+ `McpClientToolAdapter`** (`mcp/adapter.rs`, consumes stdio MCP servers); attached via `mcp::attach` in **both** CLI and daemon; `Runtime` dedups specs | config via `--mcp-config`; only a public registry API is left |
| `LlmProvider` | `nerve-agent/provider` | ✅ 3 built-in **+ config-driven** (`ProviderRegistry` + `--provider-config`; `ProviderWire` for the OpenAI-compatible long tail, no code) | promote to a named registry; otherwise done |
| `ToolBox` | `nerve-agent/provider` | `RuntimeToolBox` | fine (agent↔tools seam is wired) |
| `AuthStrategy` | `nerve-agent/auth` | 3 providers, staged (`start`/`complete`/`refresh`), driven over `auth.*` protocol | client owns callback capture (§3.7); could be config-driven |
| **Delegate / external agent CLI (primary product seam)** | `nerve-proto` (`delegate.*`) + `delegate_runtime.rs` | ✅ `RuntimeCommand::Delegate{Start,Steer,Close}` spawns sandboxed **codex / claude / gemini**, streams `DelegateProgress`, approvals via `session.respond`; steerable parked sessions | **multi-agent cockpit** (§8): run several side by side, per-thread agent binding, live agents dashboard, cross-agent context handoff |
| Session / Conversation **(own-engine; secondary)** | `nerve-runtime` + `session_manager/` | ✅ `RuntimeCommand::Session*` + `SessionManager` (multi-turn, interrupt, resume, set-model) + `ProtocolApprover` approval round-trip; run as daemon jobs | **demoted** from the product surface (§1); kept for headless/embedded; not featured in the GUI |
| Skill / AgentDef | `capabilities.rs` | ✅ `Capabilities::discover` loads agent defs **+ skills** (project > global > built-in; `BUILTIN_AGENTS` / `BUILTIN_SKILLS`) | workflow defs; a versioned on-disk schema |
| Policy / Permission | `policy.rs` | ✅ `PolicyToolBox` outermost gate (invariant 9); `policy.json` global-authoritative + project tighten-only; CLI-interactive / daemon-deny / session-protocol approvers | the orthogonal `SandboxLauncher` containment half (exec) |
| Hooks | `hooks.rs` + `nerve-agent::Hook` | ✅ wired via `Orchestrator::with_hooks` — `on_start` (environment + memory recall) and request-time checkpoint capture | further points (response/end) are additive when needed |
| Persistence | `session.rs` | ✅ `SessionStore` versioned transcripts (`schema_version` + `migrate_to_current` scaffold) under `.nerve/sessions`; resume via the session layer; credentials persisted by `nerve-agent::auth` | live daemon **jobs** stay in-memory by design; SQLite only on a measured trigger |
| Agent memory (`MemoryStore`) | `nerve-workstation` (`memory.rs`) | ✅ working-memory checkpoint (`Hook::on_request`) **and** long-term file-first (`FileMemoryStore` over `.nerve/memory.md`, `remember` tool, `on_start` recall, opt-in distillation) | promote file→SQLite on measured triggers; episodic history; recall reuses `semantic` (not a 2nd vector stack) |

## 6. Plugin architecture — layered by audience

Do not build one plugin system; layer by what is being extended, each with the right mechanism:

1. **Tools — dual track.**
   - **MCP client (highest leverage, reuses an existing port):** an `McpClientToolAdapter:
     RuntimeToolAdapter` that consumes external MCP servers — their specs flow into `tool_specs()`
     and calls proxy through. Any MCP server becomes a nerve tool with **zero recompile**, via the
     industry standard. (nerve is already an MCP server; being a client is symmetric.)
   - **First-party `RuntimeToolAdapter`** (compile-time, zero overhead): e.g. xAI.
2. **Model providers — trait + config.** `LlmProvider` for non-compatible wire formats; **config**
   for the OpenAI-compatible long tail (`{base_url, api_key, wire}` — no code).
3. **Capabilities — data as plugin.** Skills (markdown + optional scripts, discovered from
   directories) and Agent/Workflow definitions (YAML; precedence project > global > built-in).
   No recompile; versioned.
4. **Lifecycle — hooks.** Expose the orchestrator's Start/Request/Response/End lifecycle as hook
   points (memory, compaction, telemetry, policy).
5. **Data sources — `CatalogProvider`** (already a port).

> Key insight: the only genuinely *new* mechanisms needed are (1) MCP-client, (2) skill/agent-def
> loader, (3) the session layer, (4) the permission engine, (5) persistence. Everything else is
> promoting existing ports to registries/config.
>
> **Update (2026-06-19): (1)–(5) have since shipped** (see §5 / §8). What remains is promoting a few
> ports to first-class registry/config APIs and the orthogonal exec-sandbox containment half of P4.

## 7. Stable contracts to lock (so future work is additive, not breaking)

1. **Versioned runtime protocol** (`nerve-runtime`) — session/agent vocabulary added as data (v3→v4);
   never break published fields. Codegen + drift-test enforced.
2. **Provider config schema** — adding a provider = adding config.
3. **Tool / MCP registry + spec contract** — discovery, namespacing, dedup (`Runtime` already dedups).
4. **Session / Conversation model** — the missing protocol layer.
5. **Skill / AgentDef format** — versioned data contract.
6. **Persistence schema** — conversations / credentials / plugin config, migratable.
7. **Permission model** — capability declaration + authorization decision.
8. **Extract a thin `nerve-protocol` crate** when third-party Rust plugins/clients appear, so they
   depend on protocol types only, not all of `nerve-runtime`.

## 8. Roadmap (status — reconciled to code 2026-06-22; **direction updated 2026-06-23**)

- **P7 — Multi-agent cockpit over external CLIs (NEW headline direction, 2026-06-23). ◑ In progress.**
  The product's defining capability is **managing many CLI coding agents at once**: each thread bound
  to an agent (claude / codex / gemini …), several running concurrently across projects, a live
  "agents" dashboard (status / current task / pending approvals), and **cross-agent context handoff**
  built on the deterministic engine (`build_context` / repomap / semantic) plus Nerve-as-MCP-tools.
  The foundation already exists — the `delegate.*` seam + `SandboxLauncher` (P4); remaining work is
  GUI surfacing + a small, additive management vocabulary (list/observe running agents). The
  own-engine `session.*` / `agent.run` path is **demoted to a secondary seam** (kept, not featured) —
  see §1.
- **P0 — Session layer (folds in the off-protocol agent). ✅ Done.** `RuntimeCommand::AgentRun` + the
  `Session*` command family are protocol vocabulary; the daemon runs the orchestrator as a cancellable
  job emitting structured agent events; `SessionManager` adds multi-turn, interrupt, resume, and
  in-place set-model with a `ProtocolApprover` approval round-trip. `nerve agent run` shares the one
  host executor `agent::run_agent` (see §2). Invariant §3 restored; GUI/TUI can drive the agent.
- **P1 — MCP client. ✅ Done.** `McpClientToolAdapter: RuntimeToolAdapter` consumes external stdio MCP
  servers (`--mcp-config`); attached via `mcp::attach` in both faces. Near-zero new architecture.
- **P2 — Provider registry + config-driven providers. ✅ Done.** `ProviderRegistry` resolves
  built-ins + `--provider-config` entries; `ProviderWire` covers the OpenAI-compatible long tail with
  no code, and the named registry API (`descriptors`, `descriptor`, `contains_name`) lets UI/agent
  definitions validate providers without constructing clients or credentials.
- **P3 — Skills + Agent/Workflow definitions. ✅ Done.** `Capabilities::discover` loads agent defs
  and skills, `WorkerRegistry` / `WorkflowRegistry` load worker + workflow defs, and `flow.start`
  accepts inline or named workflow refs with project > global > built-in precedence.
- **P4 — Permission / policy engine. ✅ Done for authorization + MVP containment.** `PolicyToolBox`
  is the outermost gate (invariant 9). `SandboxLauncher` now backs `run_command` / delegate spawning
  with trusted-local `ProcessLauncher` and served-path `RefuseLauncher`; Linux-strong Landlock/seccomp
  remains a future backend (see `docs/designs/agent-exec-sandbox.md`).
- **P5 — Persistence + migrations. ✅ Done for transcripts/sessions.** Versioned `SessionRecord`
  (`schema_version` + a `migrate_to_current` scaffold) under `.nerve/sessions`; resume + a
  multi-session live registry; credentials persisted by `nerve-agent::auth`. Live daemon jobs stay
  in-memory by design.
- **P6 — Hooks + GUI (Tauri) / TUI / mobile. ◑ Partial.** Hooks wired (`Orchestrator::with_hooks`);
  the `nerve-tui` (Rust) client plus daemon stdio and HTTP/SSE transports are live; a minimal
  `daemon/gui.html` exists. Native Tauri GUI and mobile remain.
- **Auth broker (pairs with P6 mobile/remote). ◑ Partial.** `auth.lease` now exposes host-managed
  OAuth lease metadata: the trusted node refreshes via `AuthManager` / `ensure_fresh`, does not return
  bearer or stored refresh tokens through runtime jobs, and advertises that boundary via `auth.status`.
  The Rust TUI consumes it via `/lease`; the Leptos Web GUI sends `include_token=false`
  and shows metadata-only lease status in Settings. `auth.start.flow` now reserves `device_code` and
  fails closed when requested, while `auth.status` advertises per-provider auth capabilities so remote
  clients can discover browser/device-code/lease support without trial-and-error; the Leptos Web GUI
  now renders that capability matrix in Settings with metadata-only requests, secret-shaped reason
  redaction, and Web-GUI-boundary bearer wording. It can also run the staged browser flow by calling
  `auth.start`, opening the provider authorize URL, and submitting a pasted callback/code to
  `auth.complete`; token exchange and credential storage remain daemon-only. Remaining work: mobile
  UI, a first-party daemon callback catcher, and real provider device-authorization endpoints.

## 9. Risks & anti-goals

- **Determinism is non-negotiable.** Plugins (MCP / skills / providers) live above the kernel; never
  let one touch `nerve-core` — it would destroy golden-test reproducibility.
- **Security before openness.** Ship the permission engine + trust gates (P4) **before** enabling
  third-party MCP servers or script-bearing skills. A plugin is arbitrary code execution.
- **Versioned or dead.** Once a protocol / provider / skill contract ships, it is additive-only.
- **Don't rebuild what the agent CLIs already do — orchestrate them.** Nerve competes on
  deterministic code-intelligence + multi-agent orchestration, not on being a better single agent
  loop. Investing in the built-in `nerve-agent` engine is **not** a priority while the cockpit is the
  goal; spend effort on the `delegate.*` management surface instead.
- **Anti-goals:** no premature WASM plugin host; no bespoke plugin protocol (MCP is the standard);
  no kitchen-sink protocol (a session is not strings stuffed into `JobProgress`); no premature crate
  splitting (split only when independent versioning is needed, e.g. `nerve-protocol`).

## 10. Governance — how the invariants stay true

- **Enforced by CI today:** protocol drift (`export-runtime-protocol -- --check` + `generated_protocol_rust_artifacts_are_current`),
  determinism (golden snapshots), file/function size, `clippy -D warnings`, fmt.
- **Command-executor exhaustiveness. ✅ Done.** A literal "every command flows through
  `Runtime::handle_command`" test is unwritable because `agent.run` / `session.*` / `auth.*` /
  `flow.*` are domain-bearing host commands that the core hub intentionally refuses. The implemented
  guard is `jobs.rs::executor_for`: an exhaustive `RuntimeCommand` match plus a partition test over
  `RUNTIME_COMMAND_NAMES`, so a newly-added command must compile-time choose exactly one executor and
  cannot silently fall through to the wrong hub.
- **Per seam:** a registry + contract tests (adapter name dedup, spec shape, provider config
  validation).
- **Record contract evolution** alongside `docs/parity/` (the differential ledger style).

When this document and the code disagree, treat it as a bug in one of them: either the change skipped
a seam (fix the change) or the seam genuinely evolved (update this doc in the same PR).
