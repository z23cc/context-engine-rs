# context-engine-rs

A minimal, runnable Rust vertical slice of a snapshot-centered context engine.

## Layout

- `crates/ctx-core` — library crate for catalog scanning, immutable snapshots, the
  `CatalogProvider` port trait, fail-closed root policy, path/content search,
  `read_file`, and `get_file_tree`.
- `crates/ctx-mcp` — binary crate exposing the engine over a synchronous stdio
  JSON-RPC loop plus small CLI commands (`serve`, `doctor`, `config`, `install`).
- `crates/ctx-ffi` — C ABI (`cdylib`/`staticlib`) for embedding the engine in
  native hosts, with a cancellation surface.

The design keeps the engine centered on immutable `CatalogSnapshot` values. File
system access is behind the `CatalogProvider` port, so the core does not depend
on any GUI or live workspace store.

## Install

### Homebrew (macOS / Linux)

```bash
brew install z23cc/tap/ctx-mcp
ctx-mcp --version
```

On supported macOS this pours a prebuilt **bottle** (instant, no compiler); other
platforms build from source via a temporary Rust toolchain. Either way `ctx-mcp`
lands on your `PATH`. See [`packaging/homebrew`](packaging/homebrew/README.md) for
how bottles, releases, and versioning work.

The distributed binary includes the **semantic search** engine (`semantic_search`
tool), **on by default**. The index builds lazily on the first `semantic_search`
call: it returns BM25 results immediately while the dense index warms in the
background, downloading a local embedding model (~300 MB) once into a per-machine
cache (`~/Library/Caches/context-engine-rs` / `~/.cache/context-engine-rs`,
shared across all workspaces — not re-downloaded per project). Pass
`serve --no-semantic` to turn it off; if you never call `semantic_search`, nothing
is downloaded or built.

### Windows (Scoop)

```powershell
scoop bucket add z23cc https://github.com/z23cc/scoop-bucket
scoop install ctx-mcp
ctx-mcp --version
```

Or download `ctx-mcp.exe` straight from the
[latest release](https://github.com/z23cc/context-engine-rs/releases/latest).
Each release's Windows binary is built and tested on `windows-latest` by CI.
(winget is not published yet — it needs a `microsoft/winget-pkgs` PR.)

### From source

```bash
cargo install --path crates/ctx-mcp   # or: cargo build --release -p ctx-mcp
```

## Run

```bash
cargo run -p ctx-mcp -- doctor
cargo run -p ctx-mcp -- config roots --root "$PWD"
cargo run -p ctx-mcp -- serve --root "$PWD"
```

The stdio server expects one JSON-RPC object per line. Clients must send:

1. `initialize`
2. `notifications/initialized`
3. `tools/list` or `tools/call`

Calling tools before `notifications/initialized` returns `not initialized`.

Stdio MCP cancellation is intentionally out of scope for this synchronous loop: it
would require concurrent stdin reads and async request dispatch so a cancel
notification can be observed while a tool call is still running. Embedded hosts
that need mid-request interruption should use the C ABI cancellation surface.

## Tools

- `file_search` — path + content search with literal or regex matching, split
  `path_matches` / `content_matches`, line context, deterministic per-bucket
  top-k ordering, nucleo/Smith-Waterman fuzzy path scoring, BM25-style content
  relevance ranking, ripgrep-style smart-case, optional `whole_word`,
  binary-file skipping for content search, and real content file/byte budget
  limits (`max_content_files`, `max_content_bytes`). Scope with `extensions` and
  `include`/`exclude` globs (gitignore-style: `*.rs`, `src/**`, `**/target/**`);
  pick an `output_mode` (`content` / `files_with_matches` / `count`); and set
  asymmetric `context_before` / `context_after` (overriding `context_lines`).
- `read_file` — read a file from an allowed root with an optional line range
  (`start_line`/`end_line`, or `start_line` + `limit`; `offset` is accepted as an
  alias for `start_line`), preserving selected trailing newlines and returning
  `first_line` / `last_line` plus root-prefixed `display_path`.
- `get_file_tree` — token-efficient ASCII `tree` string plus `roots_count`,
  `was_truncated`, `uses_legend`, `omitted`, and `note` fields from the current
  catalog snapshot (the ASCII tree is the structure; no redundant nested JSON).
- `get_code_structure` — tree-sitter codemap (definitions: kind/name/line, with
  signatures and members) across Rust, Python, JavaScript, TypeScript, Go, Java,
  C, C++, C#, Ruby, and PHP. Each file carries a `token_count` and the response a
  `total_tokens`, so callers can budget which structures to include.
- `get_repo_map` — pure-Rust deterministic PageRank repo-map that ranks files
  by cross-file symbol-reference relevance, with optional `query` and
  `seed_paths` personalization and a `max_files` file budget.
- `goto_definition` / `find_references` / `call_hierarchy` — deterministic,
  syntax-level symbol navigation over the same tree-sitter tags (search-based,
  à la ctags / GitHub code nav; no language server, no external process).
  Definition/reference sites carry `kind`, signature, and an inline source-line
  snippet; `call_hierarchy` resolves callers (incoming, by enclosing definition)
  and callees (outgoing, from the symbol's block). Optional `language` filter.
  `find_references` scores each hit's `confidence` (high when the name is
  unambiguous, shares a file with a definition, or its file imports a defining
  file; low otherwise) and reports `definition_count`; `confident_only` drops the
  ambiguous hits. Still not a full scope/type resolver — aliases/re-exports and
  cross-language same-name collisions remain best-effort.
- `edit` / `write` / `delete` / `move` — root-policy-gated file mutations. `edit`
  has four modes (`replace`, `patch`, `apply_patch`, `hashline`); hashline binds
  each change to a content tag so a stale edit is refused. Responses carry a
  unified diff, tree-sitter syntax diagnostics, and a refreshed `[PATH#TAG]` view.
- `ast_search` / `ast_edit` — deterministic structural search and rewrite via
  tree-sitter queries (`@match` region, `${capture}` templates).
- `git` — read-only `status` / `diff` / `log` / `blame` / `show` in the workspace
  root.
- `manage_selection` — persistent engine-owned selection state with
  `full` / `slices` / `codemap_only` modes and per-file token estimates.
- `workspace_context` — assemble the current selection plus optional
  instructions into file-map/content text with structured token breakdowns.
- `build_context` — deterministic query-focused context builder that combines
  search, personalized repo-map ranking, greedy token-budget mode selection, and
  `workspace_context` assembly without mutating the persistent selection.
- `manage_workspaces` — add / remove / list named workspaces in the running
  server so tool calls can target a specific project by `workspace` name
  (multi-project routing).

No roots means fail-closed: catalog/read/search operations are refused. Numeric
tool parameters accept integers or integer-valued strings (e.g. `"limit": "120"`),
tolerating clients that stringify numbers.

## Embedded C ABI and cancellation

`crates/ctx-ffi` exposes a small C ABI for native hosts. The original
`ctx_engine_handle_request` remains available and runs with a never-cancel token.
Hosts that need to interrupt long-running work can create a `CtxCancel` with
`ctx_cancel_new`, pass it to `ctx_engine_handle_request_cancellable`, and call
`ctx_cancel_trigger` from another thread. A null cancel pointer means the request
cannot be cancelled. Cancelled requests return a JSON error object with
`{"error":{"kind":"cancelled",...}}`.

The cancel token is backed by Rust `Arc<AtomicBool>`, so triggering cancellation
from another thread is atomic-safe as long as the host keeps the token alive until
the cancellable request returns. Release tokens with `ctx_cancel_free`.

## Search behavior

`file_search` applies the same matching rules to path and content search:

- **Deterministic per-bucket top-k**: path and content matches admitted by the
  content file/byte budget are ranked with their own scorer, sorted by score
  descending, then by `path`, then by line where applicable, and only then
  truncated to `max_results` per bucket. Parallel content search is merged
  deterministically without forcing path and content scores onto one scale.
- **Smart-case**: a pattern with no uppercase letters is case-insensitive; a
  pattern containing any uppercase letter is case-sensitive. Regex searches use
  the equivalent of `(?i)` when smart-case selects case-insensitive matching;
  literal searches use ASCII-insensitive Aho-Corasick.
- **Path scoring**: exact substring matches outrank fuzzy matches. Fuzzy ranking
  uses the pure-Rust `nucleo-matcher` Smith-Waterman scorer with path-aware
  bonuses, smart-case case matching, and the existing `whole_word` boundary
  filter layered on top.
- **Content scoring**: literal non-`whole_word` content ranking is file-level
  BM25 over terms split on non-alphanumeric characters (`k1=1.2`, `b=0.75`,
  IDF over the searched file set). Single-term queries naturally degenerate to
  saturated term frequency plus document-length normalization. Regex and
  `whole_word` searches fall back to TF density because their match spans do not
  provide stable lexical query terms.
- **Binary content**: content search sniffs the first ~8 KiB for `NUL`; binary
  files are skipped instead of decoded with lossy UTF-8. `binary_files_skipped`
  and `diagnostics` report the skip count and paths.
- **Whole word**: `whole_word: true` requires ASCII word boundaries around path
  and content matches.

## Codemap

`get_code_structure` uses [tree-sitter](https://tree-sitter.org) with each
grammar's `tags.scm` query. Supported by extension: Rust, Python, JavaScript,
TypeScript/TSX, Go, Java, C, C++, C#, Ruby, and PHP. Adding a language is a
grammar crate plus its tags query.

The response uses the symbol model `(kind, name, line)`, where `kind` comes from
the tags query (`function`, `method`, `class`, `interface`, `module`, …). Tag
queries capture nested definitions (e.g. methods), not just the top level.
Unsupported files are omitted. Filesystem providers cache parse results by
`(mtime, size)`; `get_code_structure` and `get_repo_map` share that cache.

Because tree-sitter grammars are C, building requires a C toolchain (each
grammar's generated `parser.c` is compiled).

## Repo-map

`get_repo_map` builds an Aider-inspired repository map from the same tree-sitter
tag extraction. Each supported file is parsed once and the same pass emits both
definition and reference tags. Repo-map adds weighted directed
edges from a referencing file to same-language files defining the referenced
name, then runs deterministic sparse PageRank (`damping=0.85`, fixed 30
iterations). File nodes are sorted by path and PageRank summation is serialized
so golden rankings remain portable.

Personalization is optional: `query` seeds files whose path or content match the
literal query, and `seed_paths` seeds explicit relative or in-root absolute paths.
If no seed matches, PageRank uses a uniform restart distribution for global
importance. `max_files` truncates the ranked response; `structuredContent`
carries the ranking only (`rank`/`path`/`display_path`/`language`/`score`). The
model-facing `content[].text` renders the aider-style map (ranked file + key
symbol names) under a character budget, dropping the tail with a note when it
overflows; full signatures/fields come from `get_code_structure` on demand.

Reference edges are **tree-sitter `@reference.*` tags matched by same-language
definition name** rather than raw text occurrences. Comments and strings are
excluded naturally by parsing, while language stopwords, identifiers shorter than
three characters, high document-frequency definitions, and cross-language
same-name edges remain filtered. This is not a full scope/type resolver: import
paths, aliases, re-exports, and multi-definition disambiguation are out of scope.

## Golden snapshots and parity ledger

The fixed fixture corpus lives under `crates/ctx-core/tests/fixtures`. Golden
snapshot tests cover `file_search`, `read_file`, `get_file_tree`,
`get_code_structure`, `get_repo_map`, `workspace_context`, and `build_context`
using `insta`. The tests normalize volatile presentation fields such as the
fixture root label before snapshotting; elapsed timing and absolute paths are
not part of hard goldens.

Update snapshots intentionally with:

```bash
INSTA_UPDATE=always cargo test -p ctx-core --test golden
cargo insta review
# or accept pending snapshots with cargo insta accept
```

Cross-engine comparison against RepoPrompt headless is documented under
`docs/parity/`. It is a difference ledger and captured reference artifact, not a
hard test gate.

## Quality gates

```bash
cargo build
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```

## Performance

Measured with `cargo bench -p ctx-core --bench engine_hot_paths` (release) over a
deterministic 4096-file synthetic corpus (~14 MiB) on an 18-core machine. These are
micro-benchmarks on synthetic data; run `cargo bench` locally for your own figures.

| Operation | Time | Throughput |
|---|---|---|
| Catalog scan (parallel walk) | ~8.5 ms | ~482K files/s |
| Content search (parallel literal grep) | ~32 ms | ~445 MiB/s |
| Path search (nucleo fuzzy) | ~2.5 ms | ~1.65M paths/s |
| Repeated tool search — uncached | ~204 ms | — |
| Repeated tool search — cached | ~62 ms | ~3.3x speedup |

Notes:
- On-demand model (no persistent index by default): a cold query re-scans; the snapshot/codemap
  cache makes repeated requests within a session ~3.3x faster. The optional `semantic`
  Cargo feature keeps its code index in memory for the process/session; it does not write
  a workspace index to disk.
- Rough large-repo extrapolation: content search ~445 MiB/s is ~225 ms cold over a
  100 MB tree; catalog ~482K files/s is ~200 ms over 100K files; warm queries hit the cache.
- Same order of magnitude as ripgrep (which is faster via mmap/SIMD/streaming); the goal
  here is an embeddable on-demand engine, not to beat a dedicated grep.

## Use with Claude Code / Codex (MCP)

`ctx-mcp` is an MCP server over stdio (JSON-RPC 2.0: `initialize` / `tools/list` /
`tools/call`). It exposes all 19 default tools and is **fail-closed** — pass the project root
with `--root`. Installed via Homebrew the binary is already on your `PATH` (use
`"command": "ctx-mcp"`); otherwise build it first:

```bash
cargo build --release -p ctx-mcp   # -> target/release/ctx-mcp
```

Optional semantic retrieval is feature-gated and native-only:

```bash
cargo build --release -p ctx-mcp --features semantic
ctx-mcp serve --root /abs/project --semantic-index \
  --semantic-embedding-model jina-embeddings-v2-base-code \
  --semantic-reranker-model bge-reranker-base
```

The `semantic_search` tool builds an in-memory chunk index on first query and fuses
local embeddings, chunk-level BM25, and optional reranking. Tests use a deterministic
mock backend; real fastembed models download on first semantic use into `./.fastembed_cache`
by default, or into `--semantic-model-cache-dir`, through per-model init options rather
than a global environment variable. Use `--semantic-no-rerank` to disable reranking.
A lightweight eval harness is available:

```bash
cargo run -p ctx-core --example semantic_eval --features semantic -- crates/ctx-core/tests/fixtures "config validation"
cargo run -p ctx-core --example semantic_eval --features semantic -- /abs/sample/repo "where is auth handled" --real
```

### Automatic setup (recommended)

From a project directory, register `ctx-mcp` in Claude Code and/or Codex in one
command:

```bash
ctx-mcp install              # configure both; root = current directory
ctx-mcp install --claude     # Claude Code only
ctx-mcp install --codex      # Codex only
ctx-mcp install --dry-run    # print the commands instead of running them
```

It calls `claude mcp add` / `codex mcp add` for you (idempotent — safe to re-run),
writes an absolute `--root`, and on Homebrew uses the stable `bin/ctx-mcp` path so
the config survives upgrades. Useful flags: `--root <path>` and
`--workspace name=path` (both repeatable), `--name <server>` (default
`context-engine`), `--scope local|user|project` (Claude Code).

### Manual setup

**Claude Code** — `.mcp.json` (or `claude mcp add`):

```json
{
  "mcpServers": {
    "context-engine": {
      "command": "ctx-mcp",
      "args": ["serve", "--root", "/abs/path/to/your/project"]
    }
  }
}
```

**Codex** — `~/.codex/config.toml`:

```toml
[mcp_servers.context-engine]
command = "ctx-mcp"
args = ["serve", "--root", "/abs/path/to/your/project"]
```

Tools: `file_search`, `read_file`, `get_file_tree`, `get_code_structure`,
`get_repo_map`, `goto_definition`, `find_references`, `call_hierarchy`,
`manage_selection`,
`manage_workspaces`, `workspace_context`, `build_context`, `edit`, `write`,
`delete`, `move`, `ast_search`, `ast_edit`, `git`.
Pins MCP `protocolVersion` `2024-11-05` (clients negotiate accordingly). Codemap
covers 11 languages via tree-sitter (Rust, Python, JS, TS, Go, Java, C, C++, C#,
Ruby, PHP); building the binary requires a C toolchain.
