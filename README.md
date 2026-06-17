# context-engine-rs

A deterministic, embeddable **code-intelligence engine** exposed as an MCP server
over stdio. One pure-Rust binary gives any MCP host (Claude Code, Codex, …) fast
search, codemaps, symbol navigation, structural edits, and semantic retrieval over
a codebase — no language server, no GUI, no daemon.

## Highlights

- **27 MCP tools**: search, read, tree, codemap, repo-map, symbol nav, call
  hierarchy, structural AST search/rewrite, a 4-mode edit engine, read-only git,
  semantic search, context assembly, plus optional xAI/Grok tools when OAuth is configured.
- **Codemap over 11 languages** (tree-sitter): signatures **with return types**,
  struct/class **fields**, and full nested symbols — Rust, Python, JS, TS/TSX, Go,
  Java, C, C++, C#, Ruby, PHP.
- **Deterministic** by design: snapshot-centered, golden-tested, reproducible
  (the lexical/structural tools give the same output for the same input).
- **Hybrid search**: ripgrep-style path/content (BM25) **plus** a built-in
  semantic engine — local ONNX embeddings + ANN + cross-encoder rerank, fused via
  RRF (on by default; see below).
- **Symbol navigation** (`goto_definition` / `find_references` / `call_hierarchy`)
  with confidence scoring — the structured layer agentic coders otherwise lack.
- **Cross-platform single binary**: Homebrew bottle, Scoop, `cargo install`, or
  embed via the C ABI.

## Install

```bash
# macOS / Linux
brew install z23cc/tap/ctx-mcp

# Windows
scoop bucket add z23cc https://github.com/z23cc/scoop-bucket && scoop install ctx-mcp

# From source
cargo install --path crates/ctx-mcp
```

macOS pours a prebuilt **bottle** (instant); other platforms build from source.
See [`packaging/homebrew`](packaging/homebrew/README.md) for how bottles/releases work.

## Use with Claude Code / Codex (MCP)

One command registers `ctx-mcp` (idempotent, writes an absolute `--root`):

```bash
ctx-mcp install            # both; root = current dir   (--claude / --codex / --dry-run)
ctx-mcp warm               # optional: prebuild the current project's semantic index
ctx-mcp cache purge        # delete the current project's semantic index cache
ctx-mcp auth login xai     # optional: browser OAuth for xAI Grok subscription access
ctx-mcp auth status        # show xAI OAuth status without printing secrets
ctx-mcp auth logout        # remove stored xAI OAuth credentials
```

Or configure manually — **Claude Code** (`.mcp.json`) / **Codex** (`~/.codex/config.toml`):

```json
{ "mcpServers": { "context-engine": {
  "command": "ctx-mcp", "args": ["serve", "--root", "/abs/path/to/project"] } } }
```
```toml
[mcp_servers.context-engine]
command = "ctx-mcp"
args = ["serve", "--root", "/abs/path/to/project"]
```

The stdio loop pins MCP `protocolVersion` `2024-11-05` and is **fail-closed**: no
`--root` means catalog/read/search are refused. Numeric params also accept
integer-valued strings (e.g. `"limit": "120"`).

## xAI Grok OAuth

`ctx-mcp` can store xAI Grok OAuth credentials for integrations that want to
reuse a SuperGrok / X Premium+ browser subscription path instead of an API key:

```bash
ctx-mcp auth login xai          # opens the xAI browser OAuth PKCE flow
ctx-mcp auth login xai --force  # discard reuse and start a fresh login
ctx-mcp auth status --refresh   # refresh if expiring, then print status
ctx-mcp auth logout             # non-interactive removal
```

Tokens are stored in `~/.ctx-mcp/auth.json` by default. Set
`CTX_MCP_HOME` or `CTX_MCP_AUTH_FILE` to override the location. The stored
bearer is only sent to `https://api.x.ai/v1` or another `https://*.x.ai` URL.

The MCP server always lists Grok-backed tools, but they require `ctx-mcp auth
login xai` before use: `xai_models`, `xai_responses`, `xai_x_search`,
`xai_web_search`, `xai_image_generate`, `xai_tts`, `xai_transcribe`, and
`xai_video_generate`. Media generation tools require an explicit workspace-gated
`output_path` so large binary data is written to disk instead of returned inline.
A 403 from xAI usually means the signed-in account does not have the required
Grok/API entitlement.

## Tools

| Group | Tools |
|---|---|
| Search / read | `file_search` (path+content, BM25, smart-case, glob `include`/`exclude`/`extensions`, `output_mode`, asymmetric context; per-file cap + round-robin so one file can't monopolize results), `read_file` (line ranges, hashline view, **structural `summary` view** — signatures kept, bodies elided, with concrete re-read ranges), `get_file_tree` (budgeted ASCII tree) |
| Code intelligence | `get_code_structure` (codemap + signatures/fields + per-file `token_count`), `get_repo_map` (deterministic PageRank), `goto_definition` / `find_references` (confidence-scored) / `call_hierarchy` |
| Semantic | `semantic_search` (hybrid dense + BM25 + rerank; structured `index_state` = `ready`/`warming`/`bm25_only` + snapshot `generation` for freshness) |
| Edit | `edit` (`replace`/`patch`/`apply_patch`/`hashline`) / `write` / `delete` / `move` — root-gated, with unified diff (configurable context, optional ignore-whitespace) + syntax diagnostics; `ast_search` / `ast_edit` (structural — raw tree-sitter `query` mode **plus a `$META` pattern mode**) |
| Context / ops | `manage_selection`, `workspace_context`, `build_context`, `git` (read-only), `manage_workspaces` |
| xAI / Grok | `xai_models`, `xai_responses`, `xai_x_search`, `xai_web_search`, `xai_image_generate`, `xai_tts`, `xai_transcribe`, `xai_video_generate` |

## Semantic search (built in, on by default)

`semantic_search` works out of the box. The first call returns **BM25 results
immediately** while the dense index warms in the background, then auto-upgrades to
full hybrid (embeddings + ANN + rerank).

- **Model**: downloaded once **per machine** to `~/Library/Caches/context-engine-rs`
  (`~/.cache/...` on Linux), shared across all projects — never re-downloaded per
  directory. ~300 MB on first semantic use.
- **Index**: persisted **per project** (keyed by canonical roots + model + scope +
  version), so projects stay isolated and a warm index loads instantly.
- **Opt out**: `serve --no-semantic`. If you never call `semantic_search`, nothing
  is downloaded or built.
- **Tune**: `--semantic-cache-dir`, `--semantic-model-cache-dir`, `--semantic-no-rerank`,
  and scope flags (`--semantic-include` / `--semantic-exclude` / `--semantic-extension`).
- `build_context` automatically folds semantic candidates into its ranking when a
  warm index is available, then does a deterministic 1-hop type-reference expansion:
  files defining symbols the seed files reference are pulled in as codemap-only context.
- Structural summaries (`read_file view="summary"`) are memoized by content hash + fold options.

`build_context` / `read_file` etc. fall back gracefully when the index is cold.

## Build & quality gates

```bash
cargo build
cargo test                                    # add --features semantic for the engine
cargo clippy --all-targets -- -D warnings     # functions <=100 lines, nesting capped
cargo fmt --check
./Scripts/check-file-size.sh                  # files <=600 non-test lines
```

Building requires a C toolchain (tree-sitter grammars compile `parser.c`).
Conventions: [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md).

## Internals & design

- **Layout**: `crates/ctx-core` (engine + tools), `crates/ctx-mcp` (stdio MCP
  binary + CLI: `serve`/`doctor`/`config`/`install`), `crates/ctx-ffi` (C ABI).
- **Snapshot-centered**: filesystem access is behind a `CatalogProvider` port;
  the core operates on immutable `CatalogSnapshot` values. Codemap parses cache by
  `(mtime, size)`.
- **C ABI + cancellation**: `crates/ctx-ffi` exposes `ctx_engine_handle_request[_cancellable]`;
  a `CtxCancel` token can interrupt long requests from another thread (stdio MCP is
  synchronous and does not cancel mid-request).
- **Determinism / parity**: golden snapshots under `crates/ctx-core/tests`; the
  RepoPrompt difference ledger lives in [`docs/parity/`](docs/parity/).
- **Plans**: see [`docs/plans/`](docs/plans/).
