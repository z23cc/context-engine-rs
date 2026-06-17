# Borrowed-techniques implementation plan

Source: analysis of `oh-my-pi` and `repoprompt-ce` for techniques a deterministic,
non-LLM, pure-Rust code-intelligence MCP engine can adopt. This plan covers the two
highest-value, self-contained items. Others (ast-grep pattern mode, search per-file
cap + round-robin, content-hash summary cache, real BPE token counting, index
freshness metadata, diff-from-edit-chunks) are deferred.

All work lives in `crates/nerve-core`. Respect existing conventions:
- functions <= 100 lines, nesting capped (clippy `-D warnings`)
- files <= 600 non-test lines (`./Scripts/check-file-size.sh`)
- deterministic output, golden-tested; full data in `structuredContent`, compact text in `content[].text`
- `cargo build`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `cargo test` must pass

Existing facts (already verified):
- `crates/nerve-core/src/codemap/` parses BOTH `symbols` (definitions) and `references`
  (name-level occurrences) per file — see `codemap/types.rs` (`CodeReference`, `references` field),
  `codemap/symbols.rs`. repo-map already consumes `references`.
- `codemap/block.rs` exposes `pub(crate) fn block_span(path, source, start_line) -> Option<(usize,usize)>`
  (tree-sitter block span; brace/indent fallback). The hashline edit mode already reuses it.
- `read_file` already supports `view="hashline"`; handler in `dispatch/handlers.rs`
  (`handle_read_file`, `hashline_read_response`). Args in `dispatch/args.rs` (`ReadFileArgs.view`).
- Codemap per-file `token_count` already exists in `get_code_structure` output.

---

## [x] Item 1 (P0): `read_file` structural summary view — DONE (summarize.rs; view="summary"; 166 tests pass)

**Goal.** Add a `view="summary"` mode to `read_file` that returns a structural source
summary (declarations/signatures/import boundaries kept, bodies elided) instead of raw
lines, with an elision footer giving concrete re-read ranges. Mirrors oh-my-pi
`crates/pi-ast/src/summary.rs` (BFS unfold + per-language elidable-kind table) but
implemented in nerve-workstation's deterministic style, reusing the existing tree-sitter
codemap parse layer.

**Reference implementation to study (read-only, in `oh-my-pi` root):**
- `crates/pi-ast/src/summary.rs` — `SummaryOptions` (min_body_lines=4, min_comment_lines=6,
  unfold_until_lines, unfold_limit_lines), `SummarySegment{kind,start_line,end_line,text}`,
  `SummaryResult`, BFS unfold (`:102-153`), `is_elidable_kind` per-language table (`:396-722`),
  import-run grouping (`:277-350`), parse-failure returns full content (`:222-233`).
- oh-my-pi `packages/coding-agent/src/edit/read-file.ts` / `tools/read.ts:301-322` —
  elision footer with concrete `path:12-40,90-120` re-read ranges; collapse `{ .. }` brace pairs.

**Scope / approach.**
- New module `crates/nerve-core/src/codemap/summarize.rs` (keep < 600 lines; split if needed).
  Reuse the existing tree-sitter parse already done in `codemap/` rather than re-parsing.
  Produce ordered kept/elided segments in SOURCE ORDER with 1-based inclusive line spans.
- Elide function/method/block bodies and long multiline comments past a min-line threshold;
  KEEP signatures, type/struct headers + fields, imports (collapse middle of long import runs,
  keep first/last). Use a per-language elidable-kind approach starting from the 11 codemap langs.
- For unsupported languages or parse failure: return full content unchanged (never a partial lie).
- Optional BFS unfold to a visible-line target so it degrades under budget; default conservative
  (outermost elisions only when no target given).
- Wire `view="summary"` through `ReadFileArgs` (`dispatch/args.rs`) and `handle_read_file`
  (`dispatch/handlers.rs`); add a `summary_read_response` analogous to `hashline_read_response`.
  Render compact text with an elision footer listing real re-read ranges; put structured
  segments in `structuredContent`.
- Update the `read_file` tool description and README tool table.

**Done when.**
- `read_file {path, view:"summary"}` returns a summarized rendering of a large source file with
  signatures kept, bodies elided, and a footer naming concrete re-read line ranges.
- Unsupported/unparseable files return full content (golden-tested).
- New unit/golden tests cover: a kept-signature/elided-body case, import-run collapse, parse-failure
  fallthrough, and the footer range format. Deterministic output.
- `cargo build`, `clippy -D warnings`, `fmt --check`, `cargo test`, `check-file-size.sh` all pass.

**Key files:** `codemap/summarize.rs` (new), `codemap/mod.rs`, `dispatch/args.rs`,
`dispatch/handlers.rs`, `dispatch/text.rs`, README tool table. **Size:** large.

---

## [ ] Item 2 (P1): type-reference codemap expansion in `build_context`

**Goal.** After `build_context` selects its seed files (BM25 / PageRank / semantic), add a
deterministic 1-hop expansion: include codemap-only summaries of files that DEFINE symbols
the seed files REFERENCE. No LLM inference. Mirrors repoprompt-ce
`Features/CodeMap/CodeMapExtractor.swift:getAutoReferencedAPIs` (`:785-807`).

**Reference (read-only, in `repoprompt-ce` root):**
- `Sources/RepoPrompt/Features/CodeMap/CodeMapExtractor.swift:785-807` — build `type -> defining file`
  map over unselected files, gather `referencedTypes` from selected files, include the defining
  files (deduped by standardized path).

**Scope / approach.**
- Build a map `defined_symbol_name -> file` from codemap `symbols` of non-seed files.
- Collect referenced symbol names from seed files' codemap `references`.
- For referenced names that resolve to a defining file not already selected, add that file as a
  CODEMAP-ONLY entry (signatures, not full content), bounded by the existing token budget and a
  cap (e.g. only 1 hop, dedupe by path, deterministic ordering by name/path).
- Keep it behind the existing budget accounting so it never blows the context budget; if the
  budget is exhausted, skip expansion gracefully and note it (consistent with current degradation).

**Done when.**
- `build_context` output includes codemap-only entries for files defining symbols referenced by
  seed files, when budget allows; deterministic ordering.
- Expansion respects the token budget and degrades gracefully when exhausted.
- Tests cover: a seed file referencing a symbol defined in another file -> that file appears as
  codemap-only; budget-exhaustion skip.
- `cargo build`, `clippy -D warnings`, `fmt --check`, `cargo test` pass.

**Key files:** `crates/nerve-core/src/build_context.rs`, possibly `codemap/selection.rs`. **Size:** medium.
**Depends on:** none (independent of Item 1; different modules).

---

# Round 2 follow-ups (4 items)

Verified facts:
- Unified diff for edits is `dispatch/editing.rs::unified_diff(path, old, new)` using `similar::TextDiff::from_lines(...).unified_diff()`. `similar` (v2) is ALREADY a dependency and already groups changes into hunks. So Item 3 is a refinement, not a rewrite.
- `ast-grep` is NOT a current dependency; the engine uses tree-sitter + tree-sitter-tags directly. `ast_search`/`ast_edit` live in `dispatch/ast.rs` + `dispatch/handlers.rs` + schema in `dispatch/specs.rs`.
- `tiktoken-rs` is ALREADY a dependency (real BPE counting is available).
- Search collection is in `crates/nerve-core/src/search/` (`api.rs` collects path+content matches; `content.rs`, `matcher.rs`).
- Summary lives in `codemap/summarize.rs` (`summarize_source`, `render_summary`), called from `dispatch/handlers.rs::summary_read_response`.

## [x] Item 3 (search diversity): per-file match cap + round-robin interleave — DONE (content.rs; per-file cap + interleave; tests pass)
**Goal.** In `file_search` content results, cap matches per file and interleave across files so one
noisy file can't starve broader evidence. Mirrors oh-my-pi `search.ts` (per-file cap, round-robin
selection). Complements the already-shipped top-files header.
**Approach.** After collecting content matches in `search/api.rs`, apply a per-file cap (configurable,
sensible default) and round-robin across files up to `max_results`, deterministically (stable file
order, stable within-file line order). Surface a truncation note when a per-file cap trims hits
(reuse the existing TRUNCATED signal style). Keep `structuredContent` carrying full pre-cap totals.
**Done when.** Content matches are interleaved + per-file capped; deterministic; a test proves a
high-hit file no longer monopolizes results and ordering is stable; gates pass.
**Key files:** `search/api.rs`, maybe `models.rs`/`dispatch/args.rs` for the cap knob. **Size:** medium.

## [x] Item 4 (diff quality): configurable context + optional ignore-whitespace — DONE (DiffOptions; default unchanged)
**Goal.** Improve edit-result unified diffs: configurable context lines and an optional
"ignore whitespace-only changes" mode. Mirrors repoprompt-ce `UnifiedDiffGenerator`
(hunk gap splitting, whitespace-only pair filtering). FIRST assess what `similar`'s `unified_diff()`
already covers (it already splits hunks by context gaps) and only add what's genuinely missing.
**Approach.** In `dispatch/editing.rs::unified_diff`, expose context-line control; add an optional
whitespace-insensitive mode that drops paired add/remove lines whose whitespace-normalized content
is equal. If `similar` already handles a sub-part well, keep it and document; don't reinvent.
**Done when.** Diff supports configurable context and an ignore-whitespace mode with tests; default
behavior unchanged; gates pass. If assessment shows a sub-feature is already adequate, note it in the
PR/output rather than adding redundant code.
**Key files:** `dispatch/editing.rs`. **Size:** small.

## [x] Item 5 (caching): content-hash-keyed summary cache — DONE (summary_cache.rs; LRU 128/2MiB; 176 tests pass)
**Goal.** Cache structural summaries by `(path, content_hash, summary options)` instead of recomputing
on every `read_file view="summary"`. Mirrors oh-my-pi `read.ts` summary cache (content-hash + options
key, bounded LRU, negative results cached). Note: tree-sitter parse for summaries is pure, so this is
safe to memoize.
**Approach.** Add a bounded cache (LRU, modest cap) inside the summarize path keyed by content hash +
fold params; cache "not useful / full-content fallback" as a negative result without retaining the
full source. Keep it deterministic and thread-safe. Do not regress the existing summary output.
**Done when.** Repeated identical summary reads hit the cache (proven by a test/counter); cache key
includes content hash + options; bounded; gates pass.
**Key files:** `codemap/summarize.rs` (+ a small cache helper). **Size:** small-medium.

## [x] Item 6 (ast ergonomics): pattern mode for ast_search/ast_edit — DONE ($META sugar over tree-sitter; NO ast-grep dep; tests pass)
**Goal.** Offer an easier structural-search/rewrite syntax for LLMs than raw tree-sitter S-expression
queries. Mirrors oh-my-pi's ast-grep integration (50+ grammars, `$VAR` metavars, pattern->replacement).
**IMPORTANT ARCHITECTURE NOTE.** `ast-grep-core` would be a NEW, heavy dependency for an engine that is
deliberately pure-tree-sitter. Before adding it, EVALUATE the cost: build time, binary size, license,
determinism. If adding the dep is not clearly justified, implement a lighter "pattern" sugar that
compiles a small `$META`-style pattern down to the EXISTING tree-sitter query machinery instead, OR
report back with a recommendation before committing to the dep. Keep the existing raw-query mode intact
either way (add a mode, don't replace).
**Done when.** `ast_search` (and `ast_edit` if feasible) accept a pattern mode with `$VAR` metavars
alongside the existing query mode; deterministic; tests cover a pattern match + rewrite; gates pass.
OR: a written recommendation in the agent output if the dependency cost argues against it.
**Key files:** `dispatch/ast.rs`, `dispatch/handlers.rs`, `dispatch/specs.rs`, possibly `Cargo.toml`. **Size:** large.

---

# Round 3 — remaining minor items (audit result)

Audit found most of these are ALREADY implemented; do not redo them:
- [x] **Real BPE token counting — ALREADY DONE.** `crates/nerve-core/src/token.rs` uses `tiktoken_rs`
  (`o200k_base` → `cl100k_base` → char-estimate fallback). Consumed by `build_context.rs`,
  `workspace_context.rs`, `selection.rs`, `codemap/types.rs`. No work needed.
- [x] **Shared scan cache across discovery tools — ALREADY DONE.** `catalog.rs` `FsCatalogProvider`
  caches the `CatalogSnapshot` (TTL `snapshot_cache_ttl`, default 1000ms) shared by every tool via
  `snapshot_arc_cancellable` → `cached_snapshot`, plus a per-file codemap cache keyed by
  `FileSignature` (mtime/size). oh-my-pi's "empty-result recheck" is moot at a 1s TTL (staleness
  self-heals within a second). No work needed.

## [x] Item 7 (freshness metadata): structured index state in search responses — DONE (SemanticIndexState enum + generation in structuredContent)
**Goal.** Surface index/snapshot freshness as STRUCTURED data (not just a diagnostic string) so a
consumer can detect index lag without parsing prose. Mirrors repoprompt-ce stale-index metadata.
**Approach.**
- `semantic_search`: promote the existing "dense semantic index warming; returning BM25-only results"
  signal (see `semantic/index.rs:248-264`) to a structured field on `SemanticSearchResponse`, e.g.
  `index_state: "ready" | "warming" | "bm25_only"`. Keep the human diagnostic too.
- Optionally add the `CatalogSnapshot.generation` counter to `SearchResponse` and
  `SemanticSearchResponse` `structuredContent` so callers can correlate results across calls / detect
  a rescan. The search/semantic functions already hold the snapshot.
- Keep the compact `content[].text` essentially unchanged (this is structured-only metadata).
**Done when.** `semantic_search` returns a structured `index_state`; `generation` is exposed in
structuredContent; tests cover the warming vs ready states; deterministic; gates pass.
**Key files:** `models.rs`, `semantic/index.rs`, `search/api.rs`, `dispatch/text.rs` (if needed). **Size:** small.

---

# Round 4 — deterministic editing/read ergonomics (all DONE 2026-06-17)

Second-pass mining of `repoprompt-ce` + `oh-my-pi` for techniques not covered by Rounds 1-3.
Plan grounded in `prompt-exports/oracle-plan-2026-06-17-...-round-4-plan`. All three shipped
with `cargo build` / `clippy -D warnings` / `fmt --check` / `cargo test` (210 passing) /
`check-file-size.sh` green.

## [x] Item 8 (selection): rebase persisted slice selections across edits — DONE
New pure module `selection_rebase.rs` (`rebase_ranges` = contiguous prefix/suffix fast path +
line-window anchor fallback; drop-unresolvable; clamp to new length). Post-commit selection
maintenance centralized in `dispatch/editing.rs`; `write`/`delete`/`move`/`ast_edit` routed
through the shared hook (rebase / transfer key on rename, preserving Full/CodemapOnly /
remove on delete). Structured rebase+dropped metadata in edit output; compact one-line text.
Mirrors repoprompt-ce `SliceRebaseEngine.swift`. Tests: 6 unit + dispatch regressions +
`golden_workspace_context_rebased_slice_after_edit`.
**Files:** `selection_rebase.rs` (new), `selection.rs`, `dispatch/{editing,handlers,mod}.rs`, `tests/golden.rs`.

## [x] Item 9 (edit reliability): batch preflight + atomicity contract — DONE
Kept hashline stale-hash guard as a hard pre-mutation failure. **Deferred by design** (would
break the stateless/deterministic model): seen-lines provenance (2a) and automatic 3-way stale
recovery (2b). **Shipped:** deterministic multi-file batch preflight (`dispatch/batch.rs` —
duplicate create/update, source/dest conflicts, delete+update, rename cycles, destination
collisions; fail before any write); optional `edit.atomic` arg (default false) backed by an
additive `CatalogProvider` batch capability — MemoryCatalogProvider commits atomically,
FsCatalogProvider does best-effort temp-backup rollback (`catalog/{fs_atomic,memory_batch}.rs`),
unsupported providers fail loudly; structured StaleHash error gains `expected_hash`/`actual_hash`/
`reread_hint`. Mirrors oh-my-pi `packages/hashline` preflight (provenance/3-way intentionally dropped).
**Files:** `dispatch/{batch,args,specs,handlers,editing,error,mod}.rs`, `port.rs`, `catalog.rs` + `catalog/{fs_atomic,memory_batch}.rs`, `edit/{mod,hashline}.rs`, `security.rs`.

## [x] Item 10 (read): syntactic-boundary snapping for `read_file` — DONE
Opt-in `snap` arg (`"none"`|`"block"`, default `"none"`; ignored by hashline/summary views).
`read.rs` snaps a requested range to its enclosing tree-sitter block: reuses `codemap::block_span`
for opener lines, plus a new smallest-containing-named-node helper in `codemap/block.rs` (root
excluded) for interior lines. Unsupported lang / parse error / blank line / no block → raw range
with `snap.applied=false` + deterministic reason. Snap details (requested vs returned ranges,
boundary_lines) in `structuredContent`; raw text unchanged. Mirrors oh-my-pi `pi-ast/src/block.rs`.
Tests: read unit (none/opener/interior/unsupported/syntax-error), codemap helper, `golden_read_file_snap_block`.
**Files:** `read.rs`, `models.rs`, `codemap/{block,mod}.rs`, `dispatch/{args,specs,handlers}.rs`, `tests/golden.rs`.

---

## Deferred / not yet taken (Round 4 backlog from the same mining pass)
Lower-tier ideas surfaced but not implemented: git diff artifact-bundle export + churn-sorted
patch truncation (RPCE `GitDiffSnapshotStore`/`GitDiffPatchParsing`); embedded-language injection
+ broader language detection (oh-my-pi `pi-ast/language`); file-tree selected/codemap markers;
slice purpose-labels through range math; multi-index path resolver; `file_search` regex sanitization.
(Tree budget-fallback and hash-staleness rejection were already present — not reopened.)
