# Differential ledger: ctx-engine-rs vs RepoPrompt headless

Captured on the shared fixture corpus in `crates/ctx-core/tests/fixtures`.
Raw captures are stored in [`captures.json`](./captures.json).

This is a difference map, not a byte-for-byte parity test. The two engines expose
different schemas and are allowed to diverge while this Rust engine is still a
minimal vertical slice.

## Fixture corpus

| Path | Purpose |
|---|---|
| `alpha.rs` | Rust struct / enum / trait / inherent impl / trait impl / function plus `needle` content |
| `nested/beta.rs` | Rust type alias / const / static / module / macro in a nested directory |
| `delta.js` | JavaScript export function / classes / exported and top-level arrow bindings plus nested function skip case |
| `gamma.py` | Python top-level class / def / async def plus method and nested function skip cases |
| `notes.txt` | Text read and content-search fixture |
| `nested/info.txt` | Nested non-Rust tree/search fixture |

## Capture method

- Rust engine: `target/debug/ctx-mcp serve --root <fixture>`.
- RepoPrompt reference: `/Users/USER/WorkSpace/repoprompt-ce/.claude/worktrees/headless-eval/.build/arm64-apple-macosx/debug/repoprompt-headless` with a temporary `--state-dir`, then `config roots add <fixture> --name fixture`, then `serve`.
- JSON-RPC sequence: `initialize -> notifications/initialized -> tools/list -> tools/call`.
- Calls were sent sequentially and each response was awaited before the next call.

## Tool ledger

### `tools/list`

| Field / behavior | Rust | RepoPrompt headless | Status |
|---|---|---|---|
| Tool available | `file_search`, `read_file`, `get_file_tree`, `get_code_structure`, `get_repo_map` | Larger profile: also `bind_context`, `manage_workspaces`, `manage_selection`, `workspace_context`, `prompt` | intentional |
| `get_code_structure` availability | Always on; pure Rust `syn` | Always listed in safe read profile | align |

### `file_search`

| Field / behavior | Rust | RepoPrompt headless | Status |
|---|---|---|---|
| Literal `needle` total | 3 matches: 0 path, 3 content | 3 matches: 0 path, 3 content | align |
| Regex `pub\\s+(struct\|enum)` total | 2 content matches | 2 content matches | align |
| Path `nested` total | 2 path matches | 2 path matches | align |
| Result schema | Split `path_matches[]` and `content_matches[]`; content includes line context and `display_path` | Split `path_matches[]` and `content_matches[]`; content includes context lines, root, display path | align |
| Budget / telemetry | Enforces `max_content_files` and `max_content_bytes`; reports scanned files/bytes, limits, `exhausted`, `binary_files_skipped`, diagnostics, and `totals_are_lower_bound` | Enforces/reports content file and byte budgets plus additional budget classes and elapsed timing | align for real content budgets; extra headless budget classes intentional |
| Result ranking | Deterministic per-bucket top-k: nucleo/Smith-Waterman path relevance and file-level BM25/TF-density content relevance, each sorted independently by score/path/line after parallel merge | Deterministic ranked result set with mature relevance heuristics | closer alignment |
| Smart-case | Pattern without uppercase is case-insensitive; uppercase makes path/content matching case-sensitive | ripgrep-like smart-case behavior in RepoPrompt search surface | closer alignment |
| Binary content | Sniffs first ~8 KiB for `NUL` and skips binary files with count/diagnostic | Binary-safe content search behavior | closer alignment |
| Fuzzy path scoring | Pure-Rust `nucleo-matcher` path-aware Smith-Waterman scoring; substring hits still outrank pure fuzzy; smart-case and `whole_word` filtering are preserved | RepoPrompt path ranking uses richer path relevance than naive subsequence | closer alignment |
| Content relevance | Literal searches rank files with BM25 over the searched file set; single-term queries use TF saturation + length normalization; regex/`whole_word` use TF-density fallback | Mature search stacks use corpus-aware relevance rather than earliest-line-only ordering | closer alignment |
| Catalog count | 6 scanned files | 8 scanned entries / 7 processed entries | diverge |
| Non-deterministic fields | None currently emitted | Includes `elapsed_milliseconds` | intentional; redact from hard goldens |

Completed alignment:
1. Rust now returns split `path_matches` / `content_matches` with content context and `display_path`.
2. Rust now enforces and reports real content file/byte budgets.
3. Rust search ranking is now deterministic ranked top-k rather than insertion-order capped.
4. Rust path matching now uses `nucleo-matcher` path-aware Smith-Waterman scoring while keeping substring hits stronger than fuzzy hits.
5. Rust content ranking now uses file-level BM25/TF-IDF-style corpus statistics for literal searches, with documented TF-density fallback for regex and `whole_word`.
6. Rust path/content matching now applies ripgrep-style smart-case consistently.
7. Rust content search now skips NUL-containing binary files and reports skip counts/diagnostics instead of lossy UTF-8 searching them.

Remaining alignment options:
1. Additional headless budget classes and elapsed timing remain intentionally out of scope; elapsed stays out of Rust goldens.
2. RepoPrompt still exposes more search-schema knobs; Rust currently includes the production-critical `whole_word` option only.
3. Repo-map PageRank-style repository graph ranking is deferred-next; this change intentionally stops at path fuzzy relevance and content BM25 relevance.

### `read_file`

| Field / behavior | Rust | RepoPrompt headless | Status |
|---|---|---|---|
| Whole `notes.txt` content | Same text with trailing newline preserved | Same text with trailing newline preserved | align |
| Whole line range fields | `first_line=1`, `last_line=3`, `total_lines=3` | `first_line=1`, `last_line=3`, `total_lines=3` | align |
| Slice call used in capture | Args: `start_line=2`, `limit=1`; returns line 2 only | Uses `limit=1`, returns line 2 only | align |
| Path fields | Includes `path` plus root-prefixed `display_path=fixtures/notes.txt` | Absolute `path` plus `display_path=fixture/notes.txt` | align shape; configured root label differs |

Completed alignment:
1. Rust MCP `read_file` accepts `limit` as an alias for `end_line = start_line + limit - 1`.
2. Rust now preserves selected line trailing newlines in `content`.

Remaining alignment options:
1. Configured root naming (`fixtures` vs `fixture`) can be revisited if Rust grows persistent root display names.

### `get_file_tree`

| Field / behavior | Rust | RepoPrompt headless | Status |
|---|---|---|---|
| Tree contents | `nested/beta.rs`, `nested/info.txt`, `alpha.rs`, `delta.js`, `gamma.py`, `notes.txt` | Same entries | align |
| Root label | `fixtures` from directory basename | Configured root name `fixture` | diverge |
| Structured shape | JSON node tree plus ASCII `tree`, `roots_count`, `was_truncated`, `uses_legend`, and `omitted` | ASCII `tree`, `roots_count`, `was_truncated`, `uses_legend` | align for ASCII fields; JSON tree retained intentionally |
| Snapshot stability | Root label normalized in insta tests to `<fixture-root>` | Not in hard tests; captured as reference artifact | intentional |

Completed alignment:
1. Rust now returns ASCII `tree`, `roots_count`, `was_truncated`, and `uses_legend` while retaining JSON nodes.

Remaining alignment options:
1. Add optional configured/display root labels if the Rust engine grows persistent config.

### `get_code_structure`

| Field / behavior | Rust pure-Rust codemap | RepoPrompt `headless-lightweight` | Status |
|---|---|---|---|
| Parser | Rust: `syn`; Python: `ruff_python_parser`; JS/TS: `oxc` | `headless-lightweight` | intentional |
| Dispatch | `.rs`, `.py`, `.pyi`, `.js`, `.jsx`, `.mjs`, `.cjs`, `.ts`, `.tsx`; unsupported files omitted | Supports the captured Rust/Python/JS files and omits files with no lightweight symbols | closer alignment |
| `alpha.rs` declarations | 6 top-level symbols: struct, enum, trait, impl, impl, function | 9 symbols: type-like declarations plus trait/impl methods and function signatures | diverge |
| `nested/beta.rs` declarations | 5 top-level symbols: type, const, static, mod, macro | Skipped: no lightweight symbols returned for this file | diverge |
| `delta.js` declarations | 6 top-level symbols: export function, class, exported arrow binding, top-level arrow/function-expression bindings, export class | 7 symbols: same plus nested `function nestedSkip()` inside `if` | diverge by top-level-only rule |
| `gamma.py` declarations | 3 top-level symbols: class, def, async def | 5 symbols: same plus class method and nested function | diverge by top-level-only rule |
| Symbol shape | `(kind, name, line)` with language/path wrapper | `(kind, signature, line)` | diverge |
| Impl/class/member handling | Strictly top-level; no impl/class members or nested functions | Includes trait/impl methods, Python methods/nested defs, and JS nested functions in this capture | intentional for this Rust slice |

Completed alignment:
1. Rust codemap now covers Python and JavaScript/TypeScript with pure-Rust parsers while preserving the stable `(kind, name, line)` model.
2. Python extraction reports top-level `def` / `async def` as `function` and top-level `class` as `class`.
3. JavaScript/TypeScript extraction reports top-level function declarations, class declarations, exported declarations, and top-level `const`/`let` arrow or function-expression bindings as `function`.

Recommended alignment priority:
1. Keep Rust's top-level-only behavior for now because it matches the current design goal.
2. If RepoPrompt parity becomes a goal, add a render mode with `signature` strings and optional member/nested extraction.
3. Preserve the current `(kind, name, line)` model as the stable lightweight API.


### `get_repo_map`

| Field / behavior | Rust | RepoPrompt headless | Status |
|---|---|---|---|
| Tool available | New permanent pure-Rust MCP tool | No direct headless tool with this schema; related repo intelligence lives in Context Builder | new capability; no forced parity |
| Ranking model | Deterministic sparse PageRank over a cross-file symbol reference graph | Context Builder uses richer internal retrieval/context selection rather than exposing PageRank file scores | intentional |
| Reference extraction | Codemap definitions plus language-scoped identifier references; skips comments/strings, stopwords, high-DF symbols, and cross-language same-name edges; not scope/import aware | RepoPrompt's repo intelligence is not directly comparable through a public repo-map tool | closer approximation |
| Personalization | `query` path/content hits and explicit `seed_paths`; uniform restart when no seed matches | Context Builder accepts natural-language task prompts, not this low-level seed schema | intentional |
| Dependencies | Pure Rust; no tree-sitter, `*-sys`, or C/C++ build dependency added | RepoPrompt app has its own syntax/indexing stack | intentional |

Completed alignment/new work:
1. Rust now exposes `get_repo_map` as a new MCP tool for ranked repository maps.
2. This is a new Rust-engine capability, not a parity target; RepoPrompt headless has no direct equivalent tool surface.
3. The heuristic is documented as identifier-occurrence based and not scope/import aware.
4. Reference precision improved with comment/string-aware tokenization, stopword and high-document-frequency filtering, and same-language-only edges.
5. `get_code_structure` and `get_repo_map` share the filesystem provider's `(mtime, size)` codemap parse cache; this is a performance optimization only.

## Performance (Rust engine, measured)

Benchmarks of `ctx-engine-rs` itself, not a head-to-head vs RepoPrompt headless
(that latency comparison is not yet captured — deferred-next).
`cargo bench -p ctx-core --bench engine_hot_paths`, release, 4096-file synthetic
corpus (~14 MiB), 18-core machine.

| Operation | Time | Throughput |
|---|---|---|
| Catalog scan | ~8.5 ms | ~482K files/s |
| Content search (literal) | ~32 ms | ~445 MiB/s |
| Path search (nucleo fuzzy) | ~2.5 ms | ~1.65M paths/s |
| Repeated search: uncached -> cached | ~204 ms -> ~62 ms | ~3.3x |

Model: on-demand (no persistent index); parallel scan/search + snapshot/codemap
cache. A cold large-repo query is a few hundred ms; warm queries hit the cache.
