# context-engine-rs

A minimal, runnable Rust vertical slice of a snapshot-centered context engine.

## Layout

- `crates/ctx-core` — library crate for catalog scanning, immutable snapshots, the
  `CatalogProvider` port trait, fail-closed root policy, path/content search,
  `read_file`, and `get_file_tree`.
- `crates/ctx-mcp` — binary crate exposing the engine over a synchronous stdio
  JSON-RPC loop plus small CLI commands.

The design keeps the engine centered on immutable `CatalogSnapshot` values. File
system access is behind the `CatalogProvider` port, so the core does not depend
on any GUI or live workspace store.

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
  limits (`max_content_files`, `max_content_bytes`).
- `read_file` — read a file from an allowed root with optional line range or
  `start_line` + `limit`, preserving selected trailing newlines and returning
  `first_line` / `last_line` plus root-prefixed `display_path`.
- `get_file_tree` — compact JSON tree plus token-efficient ASCII `tree`,
  `roots_count`, `was_truncated`, and `uses_legend` fields from the current
  catalog snapshot.
- `get_code_structure` — pure-Rust lightweight codemap for Rust, Python,
  and JavaScript/TypeScript top-level symbols.
- `get_repo_map` — pure-Rust deterministic PageRank repo-map that ranks files
  by cross-file symbol-reference relevance, with optional `query` and
  `seed_paths` personalization and a `max_files` file budget.
- `manage_selection` — persistent engine-owned selection state with
  `full` / `slices` / `codemap_only` modes and per-file token estimates.
- `workspace_context` — assemble the current selection plus optional
  instructions into file-map/content text with structured token breakdowns.
- `build_context` — deterministic query-focused context builder that combines
  search, personalized repo-map ranking, greedy token-budget mode selection, and
  `workspace_context` assembly without mutating the persistent selection.

No roots means fail-closed: catalog/read/search operations are refused.

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

`get_code_structure` is always available and remains pure Rust. It dispatches by
extension: Rust files use `syn` with `proc-macro2` span locations, Python files
(`.py`, `.pyi`) use `ruff_python_parser`, and JavaScript/TypeScript files
(`.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`) use `oxc`.

The response keeps the lightweight top-level symbol model `(kind, name, line)`.
Rust reports functions, structs, enums, traits, impls, modules, constants,
statics, types, and macro definitions. Python reports top-level `def` / `async
def` as `function` and top-level `class` as `class`. JavaScript/TypeScript
reports top-level function declarations, class declarations, exported
declarations, and top-level `const`/`let` arrow or function-expression bindings
as `function`.

The codemap intentionally does not expand full syntax trees, nested modules,
class members, impl members, or nested functions. Unsupported files are omitted
from the codemap response. Filesystem providers cache codemap parse results by
`(mtime, size)`, and `get_code_structure` and `get_repo_map` share that cache;
cache hits only avoid reparsing and do not change tool output.

## Repo-map

`get_repo_map` builds an Aider-inspired repository map from the same lightweight
codemap while staying pure Rust and dependency-light. Each supported
Rust/Python/JavaScript/TypeScript file is parsed once and the same AST pass emits
both top-level definitions and reference nodes. Repo-map adds weighted directed
edges from a referencing file to same-language files defining the referenced
name, then runs deterministic sparse PageRank (`damping=0.85`, fixed 30
iterations). File nodes are sorted by path and PageRank summation is serialized
so golden rankings remain portable.

Personalization is optional: `query` seeds files whose path or content match the
literal query, and `seed_paths` seeds explicit relative or in-root absolute paths.
If no seed matches, PageRank uses a uniform restart distribution for global
importance. `max_files` truncates the ranked response, and each returned file
includes a fixed-precision PageRank score plus key codemap symbols.

Reference edges are **AST node-level same-language name matches** rather than
raw text occurrences. Rust collects `use` imports, call/method-call expressions,
and type paths; Python collects `import` / `from ... import`, calls, names, and
attributes; JavaScript/TypeScript collects import declarations, `require()`
calls, call expressions, identifiers, and static member names. Imports that
resolve to another catalog file add a higher-confidence edge. Comments and
strings are excluded naturally by parsing, while language stopwords, identifiers
shorter than three characters, high document-frequency definitions, and
cross-language same-name edges remain filtered. This is still not a full
scope/type resolver: aliases, re-exports, and multi-definition disambiguation
are intentionally out of scope. No tree-sitter, native `*-sys`, or C/C++ build
dependencies are added.

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
- On-demand model (no persistent index): a cold query re-scans; the snapshot/codemap
  cache makes repeated requests within a session ~3.3x faster.
- Rough large-repo extrapolation: content search ~445 MiB/s is ~225 ms cold over a
  100 MB tree; catalog ~482K files/s is ~200 ms over 100K files; warm queries hit the cache.
- Same order of magnitude as ripgrep (which is faster via mmap/SIMD/streaming); the goal
  here is an embeddable on-demand engine, not to beat a dedicated grep.

## WebAssembly (browser / edge)

The engine compiles to `wasm32-unknown-unknown` (no filesystem, no threads). Hosts
feed files into an in-memory catalog via the `ctx-wasm` crate, then call tools through
the same dispatch surface as native. On wasm, parallel (rayon) paths fall back to
sequential and `ignore`/filesystem code is gated out.

```bash
wasm-pack build crates/ctx-wasm --target nodejs   # or --target web / bundler
```

```js
const m = require('./crates/ctx-wasm/pkg/ctx_wasm.js');
m.feed_files(JSON.stringify([{ path: 'src/lib.rs', content: 'pub fn needle() {}' }]));
const res = m.handle_request(JSON.stringify({
  name: 'file_search', arguments: { pattern: 'needle', mode: 'content' },
}));
```

`feed_files(files_json)` replaces the in-memory catalog (`[{path, content}]`, logical
paths). `handle_request(json)` takes the same `{name, arguments}` tool-call shape as
native dispatch. Runnable example: `crates/ctx-wasm/examples/node-smoke.js`.

## Use with Claude Code / Codex (MCP)

`ctx-mcp` is an MCP server over stdio (JSON-RPC 2.0: `initialize` / `tools/list` /
`tools/call`). It exposes all 8 tools and is **fail-closed** — pass the project root
with `--root`. Build it first:

```bash
cargo build --release -p ctx-mcp   # -> target/release/ctx-mcp
```

**Claude Code** — `.mcp.json` (or `claude mcp add`):

```json
{
  "mcpServers": {
    "context-engine": {
      "command": "/abs/path/to/target/release/ctx-mcp",
      "args": ["serve", "--root", "/abs/path/to/your/project"]
    }
  }
}
```

**Codex** — `~/.codex/config.toml`:

```toml
[mcp_servers.context-engine]
command = "/abs/path/to/target/release/ctx-mcp"
args = ["serve", "--root", "/abs/path/to/your/project"]
```

Tools: `file_search`, `read_file`, `get_file_tree`, `get_code_structure`,
`get_repo_map`, `manage_selection`, `workspace_context`, `build_context`.
Pins MCP `protocolVersion` `2024-11-05` (clients negotiate accordingly). Codemap
covers Rust/Python/JS. Pure Rust, no C toolchain required.
