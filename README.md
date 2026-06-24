# Nerve Workstation

A deterministic **code-intelligence engine** exposed through two runtime adapters:
agent-facing MCP over stdio, and `nerve daemon`, a local Nerve Runtime for
frontends. One pure-Rust binary gives MCP hosts (Claude Code, Codex, …) and
runtime clients fast search, codemaps, symbol navigation, and structural edits
over a codebase — no language server or GUI required.

> **Direction (2026-06-24):** Nerve is becoming the **deterministic flight-recorder +
> execution-grounded re-verifier for fleets of external coding agents** — the `nerve daemon`
> cockpit orchestrates external CLI agents, and every run is captured as a content-addressed,
> replayable **Run** gated by a portable, signed **Verification Receipt** (the verdict is the
> org's own tests, not ours). *Court reporter, not judge.* The deterministic engine below is the
> grounding for that evidence. See [`docs/designs/trust-substrate.md`](docs/designs/trust-substrate.md).

## Highlights

- **38 MCP tools**: tool discovery, search, read, tree, codemap, repo-map,
  fuzzy symbol search, impact analysis, symbol body reads/replace/insert/import-backed rename, symbol nav, call hierarchy, structural AST search/rewrite, selection preview/promote/demote,
  a 4-mode edit engine, read-only git, context assembly, plus optional xAI/Grok tools when OAuth is configured.
- **Codemap over 11 languages** (tree-sitter): signatures **with return types**,
  class/struct members, references, shebang-based script detection, Markdown fenced-code extraction,
  and deterministic repo-map ranking — Rust, Python, JS, TS/TSX, Go, Java, C, C++, C#, Ruby, PHP.
- **Deterministic** by design: snapshot-centered, golden-tested, reproducible
  (the lexical/structural tools give the same output for the same input), with
  context/file hashes for downstream prompt-cache and dedupe keys.
- **Deterministic search**: ripgrep-style path/content with real BM25 ranking,
  fused with deterministic repo-map (PageRank) centrality — same input, same
  output, every time. (Semantic recall, if wanted, is consumed via the MCP-client
  seam, never an in-kernel vector stack — see
  [`docs/designs/code-graph.md`](docs/designs/code-graph.md).)
- **Symbol intelligence** (`symbol_search` / `analyze_impact` / `find_referencing_symbols` / `read_symbol` /
  `replace_symbol_body` / `insert_before_symbol` / `insert_after_symbol` / `rename_symbol` / `goto_definition` / `find_references` / `call_hierarchy`) with partial-name discovery,
  bounded impact analysis, referencing-symbol context, one-shot body reads, conservative symbolic edits (including import-backed rename that preserves local aliases), confidence scoring, and line:column locations — the structured layer agentic coders otherwise have to approximate with grep.
- **Agent sessions with working memory**: `update_checkpoint` keeps a bounded replace-only plan/decision note pinned into every provider request, survives compaction, and is persisted with session transcripts for stale-marked resume.
- **Cross-platform single binary**: Homebrew bottle, Scoop, or `cargo install`.

## Install

```bash
# macOS / Linux
brew install z23cc/tap/nerve-workstation

# Windows
scoop bucket add z23cc https://github.com/z23cc/scoop-bucket && scoop install nerve-workstation

# From source
cargo install --path crates/nerve-workstation

# Refresh an existing local source install after pulling updates
cargo install --path crates/nerve-workstation --force
```

macOS pours a prebuilt **bottle** (instant); other platforms build from source.
See [`packaging/homebrew`](packaging/homebrew/README.md) for how bottles/releases work.
## Use with Claude Code / Codex (MCP)

One command registers Nerve as an MCP server (idempotent, writes an absolute `--root`):

```bash
nerve install            # both; root = current dir   (--claude / --codex / --dry-run)
nerve auth login xai     # optional: browser OAuth for xAI Grok subscription access
nerve auth status        # show xAI OAuth status without printing secrets
nerve auth logout        # remove stored xAI OAuth credentials
```

Or configure manually — **Claude Code** (`.mcp.json`) / **Codex** (`~/.codex/config.toml`):

```json
{ "mcpServers": { "nerve": {
  "command": "nerve", "args": ["mcp", "serve", "--root", "/abs/path/to/project"] } } }
```
```toml
[mcp_servers.nerve]
command = "nerve"
args = ["mcp", "serve", "--root", "/abs/path/to/project"]
```

The stdio loop pins MCP `protocolVersion` `2024-11-05` and is **fail-closed**: no
`--root` means catalog/read/search are refused. Numeric params also accept
integer-valued strings (e.g. `"limit": "120"`).

## `nerve daemon` local AI Workstation Runtime

Human-facing frontends should use `nerve daemon`, the local Nerve Runtime, instead of MCP:

```bash
nerve daemon --stdio --root /abs/path/to/project
```


`nerve-runtime` is the Rust schema source of truth for the `nerve-runtime` protocol. MCP stdio is an agent-facing adapter,
the runtime daemon stdio path is a transport adapter, and the `nerve-tui` Rust terminal UI
is a client of the runtime protocol.

Protocol v3 is a small JSON-RPC 2.0 subset over newline-delimited JSON (NDJSON)
on stdio. Each stdin line is one request object; each stdout line is one response
or notification. The daemon keeps method routing separate from stdio transport so
future UDS or Named Pipe transports can reuse the same runtime router. Event
notifications use method `runtime/event`. Requests with an `id` receive a response;
notifications without `id` do not.

Stable methods:

- `runtime/info` — protocol v3 metadata and capabilities.
- `runtime/tools/list` — runtime-visible tools.
- `runtime/jobs/start` — start `{ "job_id"?: string, "command": RuntimeCommand }`.
- `runtime/jobs/get` — get `{ "job_id": string, "include_result"?: boolean }`.
- `runtime/jobs/list` — list jobs with `{ "include_terminal"?: boolean, "include_results"?: boolean, "limit"?: number }`.
- `runtime/jobs/cancel` — cooperatively cancel `{ "job_id": string }`.

`RuntimeCommand` is `{ "kind": "ping" }`, `{ "kind": "tool.list" }`, or
`{ "kind": "tool.call", "name": string, "arguments"?: object }`. These kinds
are advertised as `capabilities.jobs.commandKinds`. Clients must execute commands
through `runtime/jobs/*`; job state is in-memory and disappears
when the daemon exits. Job events are `job_started`, coarse `job_progress`,
`job_cancel_requested`, and terminal `job_completed` / `job_failed` /
`job_cancelled`. Cancellation is cooperative; core tools check cancellation, while
some adapter or network calls may only stop after the current operation returns.

Example job request:

```json
{"jsonrpc":"2.0","id":1,"method":"runtime/jobs/start","params":{"job_id":"ping-1","command":{"kind":"ping"}}}
```

Example output (timestamps shortened; background progress/terminal events can interleave with the start response after `job_started`):

```json
{"jsonrpc":"2.0","method":"runtime/event","params":{"command":"ping","job_id":"ping-1","tool_name":null,"type":"job_started"}}
{"jsonrpc":"2.0","id":1,"result":{"job":{"job_id":"ping-1","status":"running","command":"ping","tool_name":null,"created_at_ms":0,"started_at_ms":0,"updated_at_ms":0,"finished_at_ms":null,"cancel_requested":false,"result":null,"error":null}}}
{"jsonrpc":"2.0","method":"runtime/event","params":{"current":null,"job_id":"ping-1","message":"executing runtime command","stage":"executing","total":null,"type":"job_progress"}}
{"jsonrpc":"2.0","method":"runtime/event","params":{"job_id":"ping-1","type":"job_completed"}}
```

## xAI Grok OAuth

`nerve` can store xAI Grok OAuth credentials for integrations that want to
reuse a SuperGrok / X Premium+ browser subscription path instead of an API key:

```bash
nerve auth login xai          # opens the xAI browser OAuth PKCE flow
nerve auth login xai --force  # discard reuse and start a fresh login
nerve auth status --refresh   # refresh if expiring, then print status
nerve auth logout             # non-interactive removal
```

Tokens are stored under the platform config directory by default (for example
`~/Library/Application Support/nerve/auth.json` on macOS or
`$XDG_CONFIG_HOME/nerve/auth.json` on Linux). Set `NERVE_HOME` or `NERVE_AUTH_FILE` to override the location. Tokens are stored in the OS
keychain when available, with a private-file JSON fallback. The stored bearer is
only sent to `https://api.x.ai/v1` or another `https://*.x.ai` URL.

The MCP server always lists Grok-backed tools, but they require `nerve auth
login xai` before use: `xai_models`, `xai_responses`, `x_search` (preferred X
search), `xai_x_search` (explicit alias), `web_search` (preferred generic web
search), `xai_web_search` (explicit alias), `xai_image_generate`, `xai_tts`,
`xai_transcribe`, and `xai_video_generate`.
Media generation tools require an explicit workspace-gated `output_path` so large
binary data is written to disk instead of returned inline.
A 403 from xAI usually means the signed-in account does not have the required
Grok/API entitlement.

## Tools

| Group | Tools |
|---|---|
| Search / read | `file_search` (path+content, BM25, smart-case, glob `include`/`exclude`/`extensions`, `output_mode`, asymmetric context, optional invalid-regex literal fallback (`regex_fallback="literal"`); per-file cap + round-robin so one file can't monopolize results), `read_file` (line ranges, hashline view, **structural `summary` view** — signatures kept, bodies elided, supported Markdown fences summarized on host lines, with concrete re-read ranges), `get_file_tree` (budgeted ASCII tree, `mode="selected"` ignores depth so selected files stay visible, with `*` selected / `+` codemap-capable markers) |
| Code intelligence | `get_code_structure` (codemap + signatures/fields + per-file `token_count`, including no-extension scripts with supported shebangs and supported Markdown fenced code), `get_repo_map`, `symbol_search`, `read_symbol`, `analyze_impact`, `find_referencing_symbols`, `goto_definition`, `find_references`, `call_hierarchy`; navigation structuredContent includes line:column locations for definitions/references |
| Edit | `edit` (`replace`/`patch`/`apply_patch`/`hashline`) / `write` / `delete` / `move` / `replace_symbol_body` / `insert_before_symbol` / `insert_after_symbol` / `rename_symbol` (conservative same-file + import-backed symbol rename) — root-gated, with unified diff (configurable context, optional ignore-whitespace), syntax diagnostics, atomic batch preflight, stale-hash rejection, and selection rebase metadata |
| Context / ops | `tool_search` (intent search over the built-in tool catalog; works without workspace routing), `manage_selection` (full/slices/codemap-only; root-prefixed paths; preview/promote/demote; optional `auto_codemap`; slice `label`/`description` preserved through rebase), `workspace_context` (optional selected `tree` + `code` sections), `build_context` (ranked context + per-signal score breakdowns + allocation/budget trace + structured sensitive-content diagnostics), `git` (read-only; diff `detail="summary"|"files"|"patches"|"bundle"|"full"`, with churn-sorted bounded patches and structured bundle handoff metadata), `manage_workspaces` |
| xAI / Grok | `xai_models`, `xai_responses`, `x_search` (preferred), `xai_x_search`, `web_search` (preferred), `xai_web_search`, `xai_image_generate`, `xai_tts`, `xai_transcribe`, `xai_video_generate` |

## Context assembly

`build_context` assembles a ranked working set **deterministically** (BM25 + repo-map
PageRank + path), then does a 1-hop type-reference expansion: files defining symbols the
seed files reference are pulled in as codemap-only context. Its manifest reports per-signal
score breakdowns, allocation/budget trace entries, and sensitive-content findings for
included full/slice content without echoing matched secret values. Structural summaries
(`read_file view="summary"`) are memoized by content hash + fold options.

## Build & quality gates

The runtime protocol schema is generated from the `nerve-runtime` Rust types.

```bash
cargo build
cargo test                                    # whole workspace
cargo clippy --all-targets -- -D warnings     # functions <=100 lines, nesting capped
cargo fmt --check
./Scripts/check-file-size.sh                  # files <=600 non-test lines

# Protocol drift check (a Rust test asserts docs/protocol/* matches the schema)
cargo run -p nerve-runtime --bin export-runtime-protocol -- --check

# Regenerate protocol schema/constants after changing nerve-runtime protocol types
cargo run -p nerve-runtime --bin export-runtime-protocol

# TUI client smoke (no-LLM round-trip against the daemon)
cargo test -p nerve-tui
```

Building requires a C toolchain (tree-sitter grammars compile `parser.c`).
Conventions: [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md).

## Internals & design

- **Layout**: `crates/nerve-core` (engine + tools), `crates/nerve-runtime`
  (transport-neutral runtime protocol + tool-adapter composition), `crates/nerve-workstation` (stdio
  MCP adapter + CLI: `mcp serve`/`daemon`/`doctor`/`config`/`install`), `crates/nerve-tui`
  (Rust terminal UI: a runtime-protocol client of `nerve daemon`, launched by `nerve chat`).
- **Snapshot-centered**: filesystem access is behind a `CatalogProvider` port;
  the core operates on immutable `CatalogSnapshot` values. Codemap parses cache by
  `(mtime, size)`.
- **Determinism / parity**: golden snapshots under `crates/nerve-core/tests`; the
  RepoPrompt difference ledger lives in [`docs/parity/`](docs/parity/).
- **Plans**: see [`docs/plans/`](docs/plans/).
