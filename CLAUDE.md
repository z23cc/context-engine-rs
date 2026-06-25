# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Nerve Workstation is a deterministic, pure-Rust code-intelligence engine exposed through two
runtime adapters over the **same** engine: an agent-facing **MCP server over stdio**, and
**`nerve daemon`**, a local runtime for human-facing frontends. The single binary is `nerve`.

**Product direction (sharpened 2026-06-24 — governed by `docs/designs/trust-substrate.md`):** Nerve is
the **deterministic flight-recorder + execution-grounded re-verifier for fleets of external coding
agents.** The human-facing runtime is a **cockpit** that orchestrates external CLI agents (Claude Code,
Codex, Gemini CLI) through the `delegate.*` seam — but the cockpit is the **distribution body**, not
the moat. The **moat** is that every agent run is captured as a content-addressed, bit-for-bit
replayable **Run** and gated by a portable, signed **Verification Receipt** whose verdict is borrowed
from the org's own tests. **Court reporter, not judge:** Nerve proves *what an agent did, that it is
replayable, and that it cleared the org's own bar* — never that the code is "correct" (INV-R1).
Generation is the commodity Nerve orchestrates and hands its engine to (as MCP tools); the built-in
`nerve-agent` LLM loop (`session.*` / `agent.run`) stays **demoted** (INV-R4 — owning a generator
poisons neutrality). See `docs/designs/trust-substrate.md` and `architecture-north-star.md` §1/§3/§8.

## Commands

```bash
# Build
cargo build                                   # whole workspace
cargo build -p nerve-workstation --bin nerve  # just the nerve binary

# Test
cargo test --workspace
cargo test --workspace golden_build_context             # run a single test by name substring

# Golden snapshots (insta) — after an *intentional* change to tool output:
cargo insta test --review                               # or: cargo insta accept

# CI gates (all enforced; clippy runs with -D warnings)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
./Scripts/check-file-size.sh                            # files <= 600 non-test lines (hard gate)

# Runtime protocol: Rust types in nerve-runtime are the source of truth. The
# export-runtime-protocol bin emits docs/protocol/runtime-v3.*.json; a Rust drift
# test (cargo test -p nerve-runtime) fails if the committed schema is stale:
cargo run -p nerve-runtime --bin export-runtime-protocol            # regenerate docs/protocol/*
cargo run -p nerve-runtime --bin export-runtime-protocol -- --check # fail on drift (CI)

# TUI smoke: the Rust TUI client is the chat client; its smoke is a cargo test
cargo test -p nerve-tui                                 # no-LLM round-trip against the daemon

# Run the engine (note: --root is mandatory; see fail-closed below)
cargo run -p nerve-workstation --bin nerve -- mcp serve --root /abs/path/to/project
cargo run -p nerve-workstation --bin nerve -- daemon --stdio --root /abs/path/to/project
```

Building requires a C toolchain — the 11 tree-sitter grammars compile `parser.c`.

## Using the nerve MCP (this project's own tools)

When the `mcp__nerve__*` tools are connected, prefer them over raw grep/cat for
symbol- and structure-level work on this codebase — they are snapshot-backed and
deterministic (same input → same output). If they aren't connected, register the
locally built binary: `cargo build -p nerve-workstation --bin nerve` then
`./target/debug/nerve install --claude --root "$(pwd)"`.

Which tool to reach for:

| Task | Tool |
|---|---|
| Find a known string / identifier | `file_search` (path+content, BM25; `mode`, `include`/`exclude`/`extensions`, `output_mode`) |
| Skim a big file without reading it whole | `read_file` `view="summary"` (signatures kept, bodies elided, with re-read ranges) |
| Read exact lines | `read_file` with `start_line`/`end_line` (or `snap="block"` to round to a syntactic block) |
| "What's in this file / crate" | `get_code_structure` (signatures + fields + per-file token_count) |
| "Which files are central" | `get_repo_map` (deterministic PageRank; seed with `query` or `seed_paths`) |
| "Where is X defined / who calls it / how is it wired" | `goto_definition`, `find_references`, `call_hierarchy` |
| Structural match/rewrite text search can't express | `ast_search`, `ast_edit` (tree-sitter `query` mode **or** `$META` pattern mode) |
| "Where does code about <concept> live" (don't know the name) | `scout` (query → ranked `path:line-range` citations; reuses `build_context` ranking — BM25 + repo-map PageRank + path; deterministic, no LLM) |
| Edit files | `edit` (`replace`/`patch`/`apply_patch`/`hashline`), `write`, `delete`, `move` |
| Assemble a working set for a question | `build_context`, then `manage_selection` / `workspace_context` |
| Read-only history | `git` (`status`/`diff`/`log`/`blame`/`show`) |

Usage notes that bite if you don't know them:
- **Workspace routing:** pass `workspace` when more than one workspace is registered, or the call
  errors as ambiguous. Numeric params also accept integer-valued strings (`"limit": "120"`).
- **Hashline edits:** call `read_file view="hashline"` first to get the `[PATH#TAG]` header and
  1-based line numbers, then `edit mode="hashline"`. A stale tag is rejected with `StaleHash` +
  `reread_hint` — re-read with `view="hashline"` and retry. `edit` returns a unified diff + syntax
  diagnostics and is root-gated.
- **Scout vs build_context:** `scout` is the cheap "where does X live" locator — given a query it
  returns compact `path:line-range` citations (clustered from content hits; files relevant only by
  graph centrality come back file-level, no range) without pulling file bodies into context. Reach
  for `build_context` instead when you want the assembled context *text* for a question, not just
  pointers. Both are deterministic and share the same ranking; `scout` takes `query`, optional
  `max_files` (default 12) and `seed_paths`.
- **Fail-closed:** without `--root`, catalog/read/search are refused. The xAI/Grok tools are out of
  scope for code work here.

## Architecture

Eight Rust crates form a layered seam. The determinism kernel `nerve-core` depends only on the
wasm-safe protocol-vocabulary crate `nerve-proto`; above the kernel sit the siblings `nerve-runtime`
and `nerve-agent`, then the `nerve-workstation` binary; `nerve-tui`, `nerve-gui`, and `nerve-wechat`
are runtime-protocol clients of the daemon, not the engine. The long-term seam/plugin model and the
binding invariants live in `docs/designs/architecture-north-star.md` — read it before any structural
change.

- **`crates/nerve-proto`** — the **single protocol authority** and the one internal crate the kernel
  may depend on: transport-neutral, wasm-safe `serde` vocabulary (the `RuntimeCommand` / `RuntimeEvent`
  families, the declarative `flow.*` types, the advisory `RiskTier` / `ToolCapability` descriptors, the
  trust-substrate L0 shapes — `provenance` / `ledger` / `verdict` / `receipt` / `outcome` / `policy` —
  and the protocol version + method constants). Pure data with **no `nerve-core` dependency** (no
  tree-sitter / C grammars), so it compiles to `wasm32-unknown-unknown` and the WASM frontend shares
  the *exact* engine types with no codegen/TS drift. The `#[derive(JsonSchema)]`s are gated behind the
  `schema` feature (off by default; the export bin and `nerve-runtime` turn it on).

- **`crates/nerve-core`** — the engine, intentionally host-agnostic. All filesystem access goes
  through the `CatalogProvider` port (`port.rs`); operations run against immutable
  `CatalogSnapshot` values (`snapshot.rs`). This snapshot-centered design is *why* the
  lexical/structural tools are deterministic and golden-testable. Tools live here
  (search, read, tree, `codemap`, `repomap`, `navigate`, `edit`, `build_context`, `scout`).
  The transport-neutral MCP dispatch entry point is `dispatch/` (`handle_tool_call*` in
  `dispatch/mod.rs`): it takes a JSON `tools/call` params object and returns a JSON result.
  Core errors are `NerveError`; dispatch surfaces `DispatchError`. Its only internal dependency is
  `nerve-proto`, whose L0 provenance shapes it content-addresses (`provenance.rs`) — pure, so the
  kernel stays deterministic.

- **`crates/nerve-runtime`** — the runtime seam above the engine: a `WorkspaceResolver` plus
  optional capability adapters (`RuntimeToolAdapter`) plus the job/event protocol. It **re-exports
  `nerve-proto` unchanged** (so `nerve_runtime::RuntimeCommand` etc. keep resolving) and owns the
  dispatch-hub wrapper and the job/event machinery — the protocol *vocabulary* lives in `nerve-proto`,
  the *dispatch* lives here. The wire format is a JSON-RPC 2.0 subset over newline-delimited JSON; the
  schema file family is `docs/protocol/runtime-v3.*.json` (stable name) while the contract version
  constant `RUNTIME_PROTOCOL_VERSION` is now `"7"`, bumped **additively** as commands are added
  (the trust-substrate L-series went v6 → v7). The `export-runtime-protocol` bin emits the
  schema/constants; a Rust drift test (`cargo test -p nerve-runtime`) fails if the committed JSON is
  stale. Regenerate with `cargo run -p nerve-runtime --bin export-runtime-protocol` after changing
  protocol types.

- **`crates/nerve-agent`** — the LLM agent layer (sibling of `nerve-runtime`; depends only on
  `nerve-core`): the `LlmProvider` trait + Anthropic/OpenAI-Responses/xAI adapters, multi-provider
  OAuth + the single credential store (`auth/`), and the `Orchestrator` tool-use loop. It reaches
  tools only through the `ToolBox` port — never the runtime/protocol directly. Synchronous (ureq).

- **`crates/nerve-workstation`** — the `nerve` binary: two adapters over the one engine.
  1. **MCP over stdio** (`server.rs`): agent-facing, pins MCP `protocolVersion` `2024-11-05`,
     and is **fail-closed** — with no `--root`, catalog/read/search are refused.
  2. **`nerve daemon`** (`daemon/`): frontend-facing local runtime that executes commands as
     cancellable in-memory **jobs** (`jobs.rs`); job state disappears when the daemon exits.
  Also: the CLI (`cli.rs` — `mcp serve` / `daemon` / `doctor` / `config` / `auth` / `agent` /
  `install` / `chat` / `verify` / `gate` / `flow` [hidden]; `verify` re-checks a captured run against
  its sealed Receipt [L2/L4] and `gate` turns a sealed Receipt into a merge-gate decision + exit code
  [L5] — both in `commands/gate.rs`), the agent wiring (`agent.rs` — `RuntimeToolBox` bridging `Runtime`→`ToolBox`, plus
  `nerve agent run/login`), the xAI/Grok tools (`xai/`), and the xAI-only `nerve auth` alias
  (`auth/`, a thin adapter over `nerve-agent::auth`, which now owns all provider credentials).

- **`crates/nerve-tui`** — the Rust terminal UI: a runtime-protocol client of `nerve daemon`
  (no engine deps). Ships as the `nerve-tui` binary that `nerve chat` launches; `nerve-tui smoke`
  is a no-LLM round-trip (`cargo test -p nerve-tui`).

- **`crates/nerve-gui`** — the Leptos **CSR** (client-side-rendered, not SSR) web frontend; a
  `wasm32-only` client of the daemon over HTTP `/rpc` (JSON-RPC) + `/events` (SSE), never Tauri IPC.
  It depends on `nerve-proto` (without the `schema` feature) so it deserializes the *exact* engine
  protocol types. Kept out of `default-members`, so the engine's host `cargo build/test --workspace`
  never tries to compile it; build it explicitly with `trunk build` (cwd `crates/nerve-gui`). The
  committed `dist/` is the shipped bundle the daemon serves at `/app`.

- **`crates/nerve-wechat`** — a personal-WeChat (个人微信) **client surface** (sibling of `nerve-tui`,
  outside the determinism boundary). It bridges Tencent's official iLink Bot gateway (QR login →
  bearer token, HTTP long-poll) to the daemon: an inbound message maps to a `delegate.start` /
  `delegate.steer` (read-only by default) via the `NerveControl` seam, with a fail-closed
  `SenderAllowlist` so only listed WeChat ids can drive an agent. Hosted in the daemon and surfaced in
  GUI/TUI over the `wechat.*` protocol.

### Things that aren't obvious from a single file

- **Determinism & parity.** Lexical/structural tools yield identical output for identical input,
  pinned by golden snapshots in `crates/nerve-core/tests/snapshots/*.snap` (insta). Behavioral
  differences vs. RepoPrompt are tracked in `docs/parity/` (`captures.json` is historical recorded
  I/O — treat it as a fixture, not editable config).
- **Two providers.** `MemoryCatalogProvider` (in-memory) backs most tests; `FsCatalogProvider` is the
  real filesystem provider. Codemap parses are cached by `(mtime, size)`.

## Conventions (CI-enforced — see `docs/CONVENTIONS.md`, `clippy.toml`)

- **Functions ≤ 100 lines** (`clippy::too_many_lines`, denied) and **nesting ≤ 6**
  (`clippy::excessive_nesting`, denied). Split by responsibility; prefer early returns. Genuinely
  irreducible cases (static tables, generated spec blocks) may carry
  `#[allow(clippy::too_many_lines)] // reason: …` rather than being fragmented.
- **Files ≤ 600 non-test lines** (counted before the first `#[cfg(test)]`). Over the cap → split
  into a `foo/{mod.rs, ...}` module directory, not by arbitrary line count.
- Rust **edition 2024**, rust-version **1.95**.

## Architecture North Star (governing — see `docs/designs/architecture-north-star.md`)

The long-term architecture and its invariants live in `docs/designs/architecture-north-star.md`.
**Read it before any structural change.** The rules below are binding:

- **Prime directive.** Every new capability enters through a **declared seam** (port / registry /
  protocol). Never open a bespoke entry point. (The off-protocol `nerve agent run` CLI is the
  cautionary counter-example: it bypassed the runtime protocol and is scheduled — roadmap P0 — to be
  folded back in as a `RuntimeCommand`.)
- **Determinism boundary.** `nerve-core` stays pure and golden-tested. No LLM / network / wall-clock /
  plugin code in the kernel — it lives in `nerve-runtime` / `nerve-agent` / `nerve-workstation`.
- **Single dispatch hub.** All tool execution goes through `Runtime` (`handle_tool_call*` /
  `handle_command*`); never call `nerve-core` dispatch directly from a host.
- **Single protocol authority.** The runtime protocol vocabulary is defined **only** in
  `nerve-proto` (wasm-safe, zero internal deps), re-exported unchanged by `nerve-runtime`, as
  transport-neutral data, drift-checked against `docs/protocol/runtime-v3.*.json`; changes are additive
  and versioned (`RUNTIME_PROTOCOL_VERSION`). `nerve-runtime` never depends on `nerve-agent`; the binary
  translates protocol data ⇄ domain types. MCP (`server.rs`) is a *separate* external protocol — keep
  session/agent vocabulary out of it.
- **Extending the system — use the seam, don't fork an entry point:**

  | Adding… | Seam to use |
  |---|---|
  | **Managing / adding an external agent CLI (primary)** | the `delegate.*` seam (`delegate_runtime.rs`) — never a new face |
  | A first-party tool | `RuntimeToolAdapter` (in `nerve-runtime`) |
  | External / third-party tools | an MCP-client `RuntimeToolAdapter` (consume MCP servers) |
  | A model provider | `nerve_agent::provider::LlmProvider` (+ config for OpenAI-compatible) |
  | A login flow | `nerve_agent::auth::AuthStrategy` |
  | A data source | `nerve_core::CatalogProvider` |
  | Agent capabilities | Skills / Agent-Def data (loaded, not compiled) |
  | A new client surface (GUI/TUI/mobile) | the versioned runtime protocol (never a new bespoke RPC) |

- **Scout has two faces.** Besides the deterministic `scout` engine tool (no LLM), the `delegate.*`
  seam carries a read-only **scout role** (DA-7): `delegate.start { role: "scout" }` — also the
  in-chat `delegate_agent role=scout` tool and TUI `/delegate scout <agent> <query>` — runs an
  *existing* CLI agent forced read-only with an explore-and-cite prompt (`delegate_roles.rs`),
  offloading repository exploration to a cheaper model. It reuses the seam, not a new entry point.

- **Roadmap priority (headline reframed 2026-06-24):** the **headline is the trust substrate** — the
  deterministic flight-recorder + execution-grounded re-verifier (capture every delegated run as a
  replayable **Run**, gate it with a signed **Verification Receipt** = the org's own tests, land it as
  a GitHub/GitLab merge-gate; see `docs/designs/trust-substrate.md` §8). **P7 — the multi-agent cockpit
  over external CLI agents** (`delegate.*` primary; own-engine `session.*` / `agent.run` demoted) is the
  substrate's **distribution body**. First bricks: the credibility floor (`delegate.list`/`delegate.get`
  + durable resumable sessions) → L0 Run capture → Receipt + merge-gate. Prior foundation: P0 Session
  layer → P1 MCP client → P2 provider registry/config → P3 skills + agent/workflow defs → P4 permission
  engine → P5 persistence → P6 hooks + GUI/mobile. See `architecture-north-star.md` §1/§8.
