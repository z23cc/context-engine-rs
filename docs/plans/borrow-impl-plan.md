# Borrowed-techniques implementation plan

> **Positioning note (2026-06-24):** governed by `docs/designs/trust-substrate.md` — Nerve's moat is the deterministic flight-recorder + execution-grounded re-verifier (replayable **Run** + signed **Receipt**); the `delegate.*` cockpit is the distribution body. Under that thesis, this doc is an implementation plan recording borrowed deterministic engine/UX techniques; it makes no directional claim that supersedes the thesis, so only this pointer is added.

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

## [x] Item 2 (P1): type-reference codemap expansion in `build_context` — DONE (reference_expansion.rs; codemap-only 1-hop refs; tests pass)

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
Lower-tier ideas surfaced but not implemented: full tree-sitter injection-range support (oh-my-pi
`pi-ast/language`) for arbitrary mixed-language syntax. Broader embedded/source-aware language handling
is partially implemented via shebang recognition for no-extension Python/JS/TS/Ruby/PHP scripts across
codemap, summary, snap/diagnostics, and explicit AST search/edit paths, plus Markdown fenced-code codemap
symbol extraction for supported fence tags with host-line number mapping. Embedded fenced-code references now carry
optional source-language metadata so build_context/reference expansion, repo-map edges, and find_references can use
Rust/Python/etc. references hosted inside Markdown without treating them as Markdown-language references. Multi-root
path resolver support is now implemented for selection/codemap path matching via root-name/root-id prefixes while
preserving old root-relative behavior.
Git diff artifact-bundle handoff is now implemented in-memory as `git { op:"diff", detail:"bundle" }`,
with compact text plus structured changed files, bounded patch payload, and truncation metadata (no
filesystem artifact writes from `nerve-core`).
Slice purpose-labels are now implemented on `LineRange` (`label`, plus `description`/`desc` input aliases), rendered in
`workspace_context`, preserved by selection rebase, and generated by `build_context` search-hit slices.
Churn-sorted git diff patch truncation is now implemented as `git { op:"diff", detail:"patches" }`
with `max_chars`; `detail:"files"` gives a churn-sorted changed-file list; `detail:"bundle"` gives
structured review handoff data.
File-tree selected/codemap markers and selected-only tree mode are now implemented in
`get_file_tree` (`mode="selected"`, `*` selected, `+` codemap-capable); `file_search` regex
sanitization is now implemented as opt-in `regex_fallback="literal"` with a structured diagnostic.
(Tree budget-fallback and hash-staleness rejection were already present — not reopened.)

---

# Round 5 — repo-map query personalization

## [x] Item 11 (repo-map): Aider-style query term personalization — DONE

GitHub research into Aider's `aider/repomap.py::get_ranked_tags` showed a practical ranking
pattern: PageRank personalization should account for user-mentioned identifiers/path components,
not only explicit seed files. Nerve's `get_repo_map` already had deterministic PageRank and literal
`query_match`; this item adds a conservative deterministic layer that extracts query terms from
natural-language, snake_case, and CamelCase text, then seeds files whose **path components or symbol
names** match those terms.

Guardrails shipped after review:
- explicit `seed_paths` remain dominant: query-term seeds are only applied when no explicit seed
  paths are supplied (literal query matches keep the pre-existing behavior);
- reference-name matches are intentionally not term seeds because they over-prioritize consumers and
  examples over defining files;
- a small deterministic stopword list filters generic natural-language/code terms (`code`, `file`,
  `error`, `config`, `get`, `set`, etc.) to avoid flattening personalization.

Tests cover term splitting, natural-language query personalization (`payment gateway` ->
`PaymentGateway`), explicit seed precedence, and the existing repo-map suite.

**Files:** `repomap/{query,rank,mod,tests}.rs`.

---

# Round 6 — context safety diagnostics

## [x] Item 12 (build_context): deterministic sensitive-content diagnostics — DONE

GitHub research into Repomix's `src/core/security/*` showed a useful context-packing safety gate:
run a secret/security scan before emitting model-ready repository context, surface suspicious files,
and avoid logging the actual secret values. Nerve now adds a pure-Rust deterministic version in
`build_context`: the final manifest includes `sensitive_findings` for included **full**, **slice**,
and **codemap-only signature** content.

Shipped behavior:
- detects high-confidence patterns such as private-key headers, AWS access key IDs, GitHub tokens,
  OpenAI keys, Slack tokens, and generic `secret/password/token/api_key = long_value` assignments;
- findings include `path`, `display_path`, `line`, `kind`, `severity`, and a generic message;
- finding messages never include the matched secret value;
- private-key block ranges are detected from the full file, so body-only slices still warn;
- `content[].text` gets a compact non-sensitive warning when findings are present, while details stay
  in `structuredContent.manifest.sensitive_findings`;
- output remains backward-compatible: the field is omitted when no findings exist.

Tests cover standalone detector behavior, private-key slice overlap, codemap-only signature scanning,
and MCP `build_context` structured/text output.

**Files:** `build_context/{mod,sensitive}.rs`, `dispatch/{specs,tests/tool_contracts}.rs`.

---

# Round 7 — explainable context ranking

## [x] Item 13 (build_context): per-signal score breakdowns — DONE

Follow-up research across Aider/Sourcegraph-style context ranking highlighted that final context should be
reviewable, not just compact. Nerve now exposes an additive `score_breakdown` object on every
`BuildContextManifest.included[]` and `excluded[]` entry. The fields are fixed-precision strings for
byte-stable structured output and represent weighted contributions from the deterministic ranking
signals:

- `search`
- `repo_map`
- `semantic`
- `path`
- `total`
- `source` (`ranked` or `not_ranked` for non-ranked reference-expansion exclusions)
- `semantic_enabled`

The component fields are rounded weighted contributions; `total` matches the authoritative existing
`score` field. This makes it clear whether a file was selected because of lexical search, repo-map
PageRank, semantic retrieval, or path relevance, while keeping the compact `content[].text` unchanged.

Tests cover included and excluded manifest entries, assert `score_breakdown.total` matches the
existing `score` field, and assert ranked entries report `source="ranked"`.

**Files:** `build_context/{mod,explain}.rs`, `dispatch/{specs,tests/tool_contracts}.rs`, `README.md`.

---

# Round 8 — explainable context budgeting

## [x] Item 14 (build_context): allocation/budget trace — DONE

`build_context` now records an additive `allocation_trace` array in the manifest so callers can audit
how candidate allocation spent the token budget. Each ranked or reference-expansion candidate gets a
deterministic trace entry with its path/display path, score, score breakdown, allocation attempts,
final result, and reason.

Shipped behavior:
- included candidates record attempted selection modes with the rendered total token count and the
  accepted mode;
- over-budget candidates record every attempted mode and the token totals that failed the budget;
- candidates skipped after `max_files` is reached are traced as excluded with reason `max_files` and
  no attempts;
- trace entries reuse the same score-breakdown provenance as ranked included/excluded manifest rows,
  while non-ranked reference-expansion rows use `source="not_ranked"`;
- output remains backward-compatible: the field is omitted only if no ranked candidates were traced.

Tests cover included and `max_files` excluded trace entries through the MCP `build_context` contract.

**Files:** `build_context/{mod,allocation,explain}.rs`, `dispatch/{specs,tests/tool_contracts}.rs`, `README.md`.

---

# Round 9 — source-aware document summaries

## [x] Item 15 (read_file): Markdown fenced-code summary ranges — DONE

The deferred mixed-language/injection-range work is now advanced by a small deterministic seam: Markdown
fence discovery is centralized and reused by codemap extraction and `read_file view="summary"`. Supported
code fences inside Markdown are summarized with the existing language-specific structural summarizer, and
elided ranges are mapped back onto the host Markdown line numbers.

Shipped behavior:
- CommonMark-compatible opening/closing fence detection lives in `codemap/markdown.rs`;
- codemap Markdown symbol/reference extraction reuses the shared scanner, skips unsupported outer fences
  correctly, and preserves reference-only fenced snippets for repo-map/build-context expansion;
- `read_file view="summary"` on Markdown with supported fences reports `language="markdown"`, keeps the
  prose/fence delimiters, and elides long embedded code bodies using host-file ranges for re-read;
- Markdown without supported fences still falls back to full content.

Tests cover shared fence scanning, unsupported outer-fence suppression, reference-only fenced snippets,
host-line summary elision, MCP `read_file` summary output, and build-context expansion from docs-only
references.

**Files:** `codemap/{markdown,mod,symbols,summarize}.rs`, `dispatch/tests/tool_contracts.rs`, `README.md`.

---

# Round 10 — embedded structural search

## [x] Item 16 (ast_search): Markdown fenced-code AST search — DONE

Source-aware mixed-language handling now extends beyond codemap and summary output into structural search.
`ast_search` scans supported Markdown fenced-code snippets for the requested language and reports matches
against the host Markdown path/line numbers; `ast_edit` remains file-language-only to avoid unsafe nested
byte-offset rewriting semantics.

Shipped behavior:
- `codemap::EmbeddedSource` exposes deterministic embedded source snippets with language and host-line
  offset metadata;
- `ast_search` includes Markdown hosts during global scans when they may contain embedded snippets;
- query and `$META` pattern search run against the embedded snippet language, then remap result lines to
  the Markdown host file;
- CommonMark fence indentation is stripped before parsing embedded code, preserving host line numbers;
- scoped directory scans skip unsupported non-Markdown files instead of reading every unknown extension;
- supported-looking fences inside unsupported outer fences are ignored by the shared scanner.

Tests cover AST search finding a Rust function inside Markdown on the correct host line, deindenting
indented Python fences, skipping unsupported non-Markdown files in scoped directory scans, and ignoring
supported markers nested inside unsupported fences.

**Files:** `codemap/{ast,mod,markdown}.rs`, `dispatch/{handlers,specs,tests/editing_selection}.rs`, `README.md`.

---

# Round 11 — embedded snap ranges

## [x] Item 17 (read_file): Markdown fenced-code `snap="block"` — DONE

The mixed-language Markdown seam now also covers raw range reads. `read_file snap="block"` detects
when the requested host-file range sits inside a supported Markdown fenced-code body, parses the
fence body with the embedded language, and maps the selected syntactic block back to host Markdown
line numbers.

Shipped behavior:
- native source-file snapping remains unchanged;
- supported Markdown fences use the same CommonMark scanner and deindented fence source as codemap,
  summary, and AST search;
- opener/interior-line block snapping works inside fenced Rust/Python/etc. examples;
- returned content stays the original Markdown host-file slice (including original indentation), not a
  synthetic deindented snippet;
- Markdown prose outside supported fences keeps the existing unsupported-language fallback reason.

Tests cover Rust fenced-code interior snapping, CommonMark-indented Python fences, and prose fallback.

**Files:** `read.rs`, `codemap/{block,mod}.rs`, `dispatch/specs.rs`.

---

# Round 12 — embedded syntax diagnostics

## [x] Item 18 (edit/write): Markdown fenced-code syntax diagnostics — DONE

Edit and write results now validate supported Markdown fenced-code snippets with the embedded language
parser, so broken code examples are reported in the same `diagnostics` array as native source syntax
errors while preserving host Markdown line numbers.

Shipped behavior:
- native syntax diagnostics are unchanged;
- Markdown diagnostics reuse the shared fence scanner and deindented source mapping;
- messages are language-qualified, e.g. `rust fenced code: syntax error`, because `SyntaxIssue` carries
  only line/message today;
- global truncation keeps the existing deterministic 20-issue cap.

Tests cover pure codemap host-line remapping and the MCP `write` tool surfacing fenced-code diagnostics
in both structuredContent and compact text.

**Files:** `codemap/{block,mod}.rs`, `dispatch/tests/editing_selection.rs`.

---

# Round 13 — tool discovery

## [x] Item 19 (dispatch): intent search over the tool catalog — DONE

Oh-my-pi's hidden-tool index (`search_tool_bm25`) highlights a practical scaling issue for large agent
surfaces: models need a compact way to discover the right capability without rereading every schema.
Nerve now ships a deterministic `tool_search` tool over its own MCP tool catalog.

Shipped behavior:
- `tool_search { query, max_results }` ranks tools by matches in tool name, description, parameter
  names, and parameter descriptions;
- it runs before workspace resolution, so it works even when multiple workspaces are registered and no
  `workspace` argument is supplied;
- compact text lists ranked matches and key params; full match details remain in `structuredContent`;
- numeric `max_results` keeps the existing lenient integer-string behavior.

Tests cover listing, multi-workspace no-workspace dispatch, compact text, structured matches, and
integer-string `max_results`.

**Files:** `dispatch/{tool_search,args,mod,specs}.rs`, `dispatch/tests/tool_contracts.rs`, `README.md`.

---

# Round 14 — symbol discovery

## [x] Item 20 (navigate): deterministic fuzzy symbol search — DONE

Codanna's `search_symbols` and Narsil's workspace-symbol discovery point at the same gap in Nerve's
syntax-level navigation: exact `goto_definition` is powerful once the model knows the symbol name,
but agents need a deterministic discovery step when they only know partial intent. Nerve now ships a
first-party `symbol_search` tool over the shared tree-sitter codemap/repo-map index.

Shipped behavior:
- `symbol_search { query, language?, kind?, max_results }` ranks symbols by case-insensitive partial
  matches across symbol names, kinds, signatures, members, and paths;
- results include path, display path, line, kind, language, score, matched terms, signature, and a
  trimmed source-line snippet where available;
- sorting is deterministic: score desc, then display path, line, kind, and name;
- `max_results` accepts the existing lenient integer-string decoding and honors `0` by returning
  totals only;
- compact text summarizes ranked symbols while full details remain in `structuredContent`.

Tests cover partial-name ranking, language/kind filters, zero-limit totals, MCP listing, dispatch,
integer-string `max_results`, and compact text.

**Files:** `navigate/{mod,symbol_search}.rs`, `dispatch/{args,handlers,specs,text}.rs`,
`dispatch/tests/tool_contracts.rs`, `lib.rs`, `README.md`.

---

# Round 15 — selection ergonomics

## [x] Item 21 (manage_selection): preview/promote/demote operations — DONE

RepoPrompt CE, rp-mini, and Synapse all treat context selection as a first-class state machine: agents
can preview token impact before committing, promote lightweight structure-only entries to full files,
and demote full files back to codemap-only when the budget gets tight. Nerve's selection tool now
adds the same deterministic ergonomics while preserving the existing `files` + `total_tokens` response.

Shipped behavior:
- `op="preview"` returns a dry-run summary without mutating provider-owned selection; with targets it
  previews adding those targets using the supplied `mode`/`slices`, and without targets it previews the
  current selection;
- `op="promote"` converts selected targets to `full`; without explicit targets it promotes all selected
  files;
- `op="demote"` converts selected targets to `codemap_only`; without explicit targets it demotes all
  selected files;
- response metadata adds `preview`, `mutated`, and `would_mutate` only when true, so existing clients can
  keep reading `files` and `total_tokens` unchanged;
- schema/docs advertise the expanded operation enum.

Tests cover non-mutating preview, persisted-state checks after preview, target/all-selected mode
conversion, schema listing, MCP dispatch, and structured mutation metadata.

**Files:** `selection.rs`, `dispatch/{specs,tests/tool_contracts}.rs`, `README.md`.

---

# Round 16 — context cache keys

## [x] Item 22 (workspace_context): deterministic content hashes — DONE

rp-mini's design calls out content-hashed workspace-context snapshots so clients can dedupe exports and
make downstream prompt caches hit reliably. Nerve now exposes deterministic hashes in the structured
`workspace_context` response without duplicating the rendered context body.

Shipped behavior:
- top-level `context_hash` hashes the exact final `content[].text` payload, including optional token
  reports when requested;
- every `tokens.files[]` entry includes `content_hash` for that file's rendered context block, so
  clients can cache or compare individual full/slice/codemap selections;
- hashes are stable 32-hex deterministic identifiers, not advertised as cryptographic digests;
- existing `context` remains model-facing text only (`skip_serializing`), preserving the compact
  structuredContent contract.

Tests cover hash shape, per-file hashes, stability across repeated calls, and hash changes when the
rendered context changes.

**Files:** `workspace_context.rs`, `dispatch/specs.rs`, `README.md`.

---

# Round 17 — selection dependency ergonomics

## [x] Item 23 (manage_selection): opt-in automatic codemap expansion — DONE

RepoPrompt CE and rp-mini both reduce manual context bookkeeping by adding lightweight codemaps for
files that define symbols referenced by selected full/slice files. Nerve now offers the same ergonomics
as an explicit opt-in so deterministic, hand-tuned selection budgets remain unchanged by default.

Shipped behavior:
- `auto_codemap: true` on `op="add"`, `op="set"`, or `op="preview"` with `full`/`slices` targets scans
  the current request's full/slice target references and adds up to eight matching same-root definition files as `codemap_only`;
- `codemap_only`, `promote`, `demote`, `remove`, `clear`, and `get` remain manual and never auto-expand;
- expansion matches existing build-context reference logic: type-like definitions only, same language
  family, and embedded Markdown fenced-code references can point at real source definitions;
- responses include `auto_codemap_added` only when non-zero.

Tests cover the positive embedded-Markdown→Python definition case plus the off-by-default,
codemap-only, and multi-root isolation guard rails.

**Files:** `selection.rs`, `selection_auto_codemap.rs`, `dispatch/{specs,tests/tool_contracts}.rs`, `README.md`.

---

# Round 18 — richer workspace-context packaging

## [x] Item 24 (workspace_context): selected tree and code sections — DONE

RepoPrompt CE's MCP workspace-context builder can include both the selected file tree and selected code
structure in the same context snapshot (`include=["tree","code"]`), so agents can receive repository
orientation, API signatures, and file contents without making separate tool calls. Nerve now adds the
same deterministic packaging as explicit opt-in sections while preserving default `file-map` + `contents`
behavior.

Shipped behavior:
- `WorkspaceContextInclude` accepts `tree` and `code` in addition to existing sections;
- `tree` renders a selected-only file tree using the same `*` selected / `+` codemap-capable markers as
  `get_file_tree mode="selected"`, including an inline marker legend;
- `code` renders codemaps for selected codemap-capable files using display paths so duplicate relative
  paths across roots remain distinguishable, omitting unsupported selections with a compact deterministic note;
- section order is stable: file map, selected tree, selected code structure, contents, diff,
  meta-prompts, instructions;
- token accounting adds `tree_tokens` and `code_tokens` only when non-zero.

Tests cover explicit tree/code context rendering, schema exposure, dispatch behavior, token fields,
marker legends, and duplicate relative paths across roots.

**Files:** `workspace_context.rs`, `workspace_context/sections.rs`, `dispatch/{specs,tests/tool_contracts}.rs`, `README.md`.

---

# Round 19 — deterministic symbol-body reads

## [x] Item 25 (navigate): `read_symbol` exact symbol body retrieval — DONE

Serena's `FindSymbolTool(include_body=true)` highlights a common agent need between fuzzy symbol
search and raw file reads: once a model knows the exact symbol name, it should be able to fetch the
one relevant enclosing definition body without first calling `goto_definition` and then manually
calculating a line range. Nerve now adds the same ergonomics in its deterministic, no-LSP navigation
layer.

Shipped behavior:
- new `read_symbol { symbol, path?, language?, kind?, include_body=true, max_matches=20 }` MCP tool;
- exact, case-sensitive symbol matching over the existing tree-sitter codemap definitions;
- optional path, language, and kind filters for disambiguation;
- when exactly one match remains and `include_body` is true, returns the enclosing parsed/fallback block
  as `body` plus compact model-facing text;
- when zero or multiple matches remain, returns candidate locations only, with a note telling callers to
  refine scope instead of guessing;
- structuredContent always includes `matches`, `total`, `truncated`, and the navigation caveat.

Tests cover a single symbol body read, ambiguous duplicate candidates, path-scoped duplicate resolution,
location-only reads (`include_body=false`), public-API clamping for `max_matches=0`, MCP schema listing,
dispatch, integer-string `max_matches`, and compact text rendering.

**Files:** `navigate/{mod,read_symbol}.rs`, `dispatch/{args,read_symbol_args,handlers,specs,text}.rs`,
`dispatch/tests/tool_contracts.rs`, `lib.rs`, `README.md`.

---

# Round 20 — deterministic impact analysis

## [x] Item 26 (navigate): `analyze_impact` bounded reverse dependency graph — DONE

Codanna's MCP surface includes `analyze_impact` for seeing what depends on a symbol across calls,
type usage, composition, and dependency relationships. Nerve now adds the deterministic subset that fits
its no-LSP tree-sitter architecture: a bounded reverse graph of enclosing symbols that reference the seed
symbol, then symbols that reference those symbols.

Shipped behavior:
- new `analyze_impact { symbol, path?, language?, kind?, max_depth=2, max_results=200, confident_only=false }` MCP tool;
- exact, case-sensitive seed symbol matching over existing codemap definitions, with optional seed
  `path`/`language`/`kind` filters;
- BFS over reverse name references up to `max_depth`; each impacted node reports symbol, path, line,
  depth, `via_symbol`, reference line/kind, confidence, and a trimmed signature/source line;
- exact target identity is preserved for recursive hops by path+line, while references remain honest
  syntactic name matches;
- structuredContent includes seed definitions, impacted nodes, total/truncation metadata, and the
  navigation caveat.

Tests cover direct and recursive dependents, recursive ambiguity confidence, self-recursive seed exclusion,
`max_depth`, result truncation, MCP schema listing, dispatch, integer-string `max_depth`/`max_results`,
and compact text rendering.

**Files:** `navigate/{mod,impact}.rs`, `dispatch/{impact_args,handlers,specs,text}.rs`,
`dispatch/tests/tool_contracts.rs`, `lib.rs`, `README.md`.

---

# Round 21 — deterministic symbolic editing

## [x] Item 27 (edit): `replace_symbol_body` whole-definition replacement — DONE

Serena's symbolic editing workflow uses `replace_symbol_body` after `find_symbol(..., include_body=true)`
when an agent needs to replace an entire function/class/method definition. Nerve now adds the same
high-level operation in its deterministic style: exact codemap lookup for one unambiguous symbol, then
root-gated file mutation through the existing edit/diff/diagnostics/selection-rebase path.

Shipped behavior:
- new `replace_symbol_body { symbol, path?, language?, kind?, body }` MCP tool;
- reuses `read_symbol` selection semantics, so zero/ambiguous matches produce a non-mutating response with
  candidate locations and refinement guidance;
- replaces the full enclosing definition line span for exactly one match;
- strips only leading/trailing blank lines from the replacement body, preserving indentation and normalizing
  line endings to the target file;
- verifies the current span still matches the symbol body found by `read_symbol` before writing, returning a
  non-mutating stale-span response if the file changed;
- writes through `apply_content_update_with_old`, so responses include unified diff, hashline view/tag,
  syntax diagnostics, and selection rebase metadata just like other edit tools.

Tests cover unique symbol replacement, neighboring-symbol preservation with and without trailing newline,
ambiguous no-op behavior, MCP schema listing, and compact diff text.

**Files:** `dispatch/{symbol_edit,handlers,specs,mod}.rs`, `dispatch/tests/{editing,tool_contracts}.rs`,
`README.md`.

---

# Round 22 — deterministic symbolic insertion

## [x] Item 28 (edit): `insert_before_symbol` / `insert_after_symbol` definition insertion — DONE

Serena's symbolic insertion workflow adds content immediately before or after an existing symbol definition
when an agent needs to add neighboring functions, methods, classes, imports, or constants without brittle
string matching. Nerve now adds the same operation in its deterministic style: exact codemap lookup for one
unambiguous symbol, stale-span verification against the current file, then root-gated file mutation through
the existing edit/diff/diagnostics/selection-rebase path.

Shipped behavior:
- new `insert_before_symbol { symbol, path?, language?, kind?, body }` and
  `insert_after_symbol { symbol, path?, language?, kind?, body }` MCP tools;
- reuses `read_symbol` selection semantics, so zero/ambiguous matches produce a non-mutating response with
  candidate locations and refinement guidance;
- inserts before the matched symbol's start line or after its end line for exactly one match;
- normalizes inserted content line endings to the target file and adds a trailing newline when needed;
- handles symbol-at-EOF insertion even when the original file has no trailing newline;
- verifies the current span still matches the symbol body found by `read_symbol` before writing, returning a
  non-mutating stale-span response if the file changed;
- writes through `apply_content_update_with_old`, so responses include unified diff, hashline view/tag,
  syntax diagnostics, and selection rebase metadata just like other edit tools.

Tests cover before/after insertion, symbol-at-EOF insertion without a trailing newline, explicit leading-newline
and empty-body EOF edge cases, multi-root root-scoped targeting, ambiguous no-op behavior, MCP schema listing,
and action metadata in edit responses.

**Files:** `dispatch/{symbol_edit,handlers,specs}.rs`, `dispatch/tests/{editing,tool_contracts}.rs`,
`README.md`.

---

# Round 23 — referencing-symbol context

## [x] Item 29 (navigate): `find_referencing_symbols` enclosing-reference view — DONE

Serena's `FindReferencingSymbolsTool` returns the symbols that reference a target plus a short snippet around
the exact reference, while Codanna exposes a separate caller-oriented surface next to raw references and impact
analysis. Nerve now adds the deterministic subset that fits its no-LSP architecture: exact target definition
filtering, name-based reference matching, enclosing-symbol resolution, confidence scoring, and compact
reference context.

Shipped behavior:
- new `find_referencing_symbols { symbol, path?, language?, kind?, confident_only=false, context_lines=1, max_results=200 }` MCP tool;
- target definitions can be narrowed by path, language, and kind using the same root/display-path conventions as
  `read_symbol` and `analyze_impact`;
- each result reports the enclosing symbol, declaration line/kind/language, exact `reference_line`,
  `reference_kind`, confidence, declaration text, trimmed reference text, and numbered source context around
  the reference;
- low-confidence ambiguous name-only matches are surfaced honestly and can be filtered with `confident_only`;
- context is bounded (`context_lines` capped at 5) and deterministic; `max_results` truncates with metadata;
- compact text gives an agent a quick caller-like list while structuredContent carries full reference context.

Tests cover enclosing-symbol resolution with reference context, ambiguous low-confidence filtering,
truncation and `context_lines=0`, MCP schema listing, dispatch, integer-string args, and compact text rendering.

**Files:** `navigate/{mod,referencing_symbols}.rs`, `dispatch/{referencing_symbols_args,handlers,specs,text,mod}.rs`,
`dispatch/tests/tool_contracts.rs`, `lib.rs`, `README.md`.

---

# Round 24 — source-precise symbol locations

## [x] Item 30 (codemap/navigate): line:column metadata for definitions and references — DONE

Rope's rename refactoring pipeline first resolves a target identity and then only rewrites filtered occurrence
ranges. Nerve is intentionally no-LSP and cannot safely perform full semantic rename yet, but it now adds the
missing deterministic foundation: codemap definitions and references carry exact 1-based byte-column positions,
and navigation responses propagate those columns to callers.

Shipped behavior:
- `CodeSymbol` and `CodeReference` now include `column`, computed from tree-sitter tag `name_range` so the
  location points at the symbol/reference name rather than the whole declaration node;
- `goto_definition`, `find_references`, `read_symbol`, `analyze_impact`, `find_referencing_symbols`, and
  `call_hierarchy` structuredContent expose definition/reference line:column metadata;
- compact navigation text renders line:column for definitions, references, impact nodes, referencing-symbols,
  and call hierarchy edges;
- this remains an additive metadata change and lays the groundwork for a future conservative `rename_symbol`
  tool without doing unsafe line-only replacement today.

Tests cover direct `find_referencing_symbols` columns, same-line duplicate reference columns, Markdown indented-fence
host columns, Unicode byte-column semantics, and MCP dispatch columns for referencing-symbol and impact responses.

**Files:** `codemap/{types,symbols}.rs`, `navigate/{mod,definition,references,read_symbol,impact,referencing_symbols,call_hierarchy}.rs`,
`dispatch/text.rs`, `dispatch/tests/tool_contracts.rs`, `README.md`.

---

# Round 25 — conservative symbolic rename

## [x] Item 31 (dispatch/edit): `rename_symbol` safe same-file refactor — DONE

Rope's mature rename flow resolves one target and rewrites only confirmed occurrences. Nerve now ships the
safe no-LSP subset: a deterministic `rename_symbol` tool that uses codemap line:column occurrences, refuses
ambiguous definitions, verifies every occurrence against current file content before writing, and avoids
cross-file name-only rewrites until import-path extraction can prove the relationship.

Shipped behavior:
- new `rename_symbol { symbol, new_name, path?, language?, kind? }` MCP tool;
- resolves exactly one old definition through `read_symbol` filters, including root-prefixed paths in multi-root
  workspaces;
- renames the definition plus same-file syntax-level references, grouped by absolute edit target while
  response paths stay display/root-qualified;
- rejects empty, punctuation-bearing, digit-prefixed, and common keyword new names;
- returns structured non-mutating no-ops for invalid names, same-name requests, ambiguous definitions,
  truncated reference scans, and stale line:column occurrences;
- applies multi-file-ready content updates through an atomic batch helper with preflight and selection rebase
  metadata.

Tests cover same-file duplicate references, unimported cross-file name-only references staying unchanged,
ambiguous no-op behavior, invalid/keyword/same-name no-ops, Unicode byte-column replacement, multi-root
root-scoped targeting, MCP schema listing, and file-size compliance.

**Files:** `dispatch/{symbol_rename,symbol_edit,editing,handlers,specs,mod}.rs`,
`dispatch/tests/{editing,tool_contracts}.rs`, `README.md`.

---

# Round 26 — import-backed symbolic rename

## [x] Item 32 (codemap/dispatch): import-path facts for safe cross-file rename — DONE

Round 25 deliberately refused cross-file name-only rewrites. This round adds the missing proof signal for the
safe subset: codemap extraction now records lightweight import references with `import_path`, and `rename_symbol`
uses the existing repo-map import resolver to expand rename scope only to files that explicitly import the target
definition file in a single-root workspace.

Shipped behavior:
- codemap now augments tree-sitter tag references with lightweight import facts for Rust `use`, Python
  `from ... import ...`, and JavaScript/TypeScript named `import { ... } from ...` statements;
- import references carry 1-based byte columns and `import_path`, preserving existing `CodeReference` serde
  compatibility with a defaulted optional field;
- `rename_symbol` now renames definition-file occurrences plus import-backed cross-file occurrences when the
  import resolver proves the importer points at the selected definition file;
- unimported cross-file name-only references remain untouched, and multi-root workspaces stay same-file-only to
  avoid duplicate relative-path ambiguity;
- tool schema and README now describe same-file + import-backed rename rather than overpromising full semantic
  rename.

Tests cover Rust import-backed rename (`use crate::target::helper`) rewriting both the import specifier and call
site while leaving an unimported `helper()` in another file unchanged; shadowed importers no-op; commented imports
do not expand scope; Unicode-prefixed import paths keep byte-column replacement correct; existing same-file,
invalid-name, no-op, ambiguous, and multi-root conservative cases still pass.

**Files:** `codemap/{imports,symbols,types,mod}.rs`, `dispatch/symbol_rename.rs`,
`dispatch/tests/editing.rs`, `dispatch/specs.rs`, `README.md`.

---

# Round 27 — alias-aware import-backed rename hardening

## [x] Item 33 (codemap/dispatch): alias-safe import-backed rename — DONE

Round 26 proved basic import-backed rename. Round 27 hardens that subset so aliased imports preserve local
alias semantics and cannot accidentally rewrite unrelated same-name occurrences in the importing file.

Shipped behavior:
- Rust single alias imports (`use crate::target::helper as h`) normalize the import fact to source name
  `helper` and source path `crate::target::helper`;
- Rust grouped aliases (`use crate::target::{helper as h, other}`), Python aliases
  (`from target import helper as h`), and JS/TS named aliases (`import { helper as h } from './target'`) rename
  only the imported source specifier while preserving `h()` call sites;
- aliased importer files are allowed into rename scope, but non-import old-name occurrences inside those files
  are filtered out unless they are in the definition file itself;
- Rust/JS `/* ... */` block comments and Python triple-quoted strings are stripped by the import fact scanner so
  fake imports in comments/strings do not expand rename scope;
- import/shadowing scope helpers were split into `dispatch/symbol_rename_scope.rs` to keep source files under the
  600-line gate.

Tests cover Rust grouped imports, Rust simple alias imports with an unrelated `helper` parameter/call, Rust grouped
alias imports, Python alias imports, JS alias imports, block-commented imports, Python triple-quoted fake imports,
and all prior Round 26 rename safety cases.

**Files:** `codemap/imports.rs`, `dispatch/{symbol_rename,symbol_rename_scope,specs}.rs`,
`dispatch/tests/editing.rs`, `README.md`.

---

# Round 28 — working-memory checkpoint persistence completion

## [x] Item 34 (agent/session): persist non-empty working-memory checkpoints — DONE

GenericAgent's `key_info` pattern and oh-my-pi's anti-junk memory discipline both rely on a bounded,
agent-curated note that is re-injected rather than reconstructed from raw logs. Nerve already had the
`update_checkpoint` tool, `CheckpointHook`, and interactive session resume path; this round closes the
remaining persistence gap for one-shot agent runs and removes empty-checkpoint noise from session records.

Shipped behavior:
- `checkpoint_snapshot` centralizes the persisted checkpoint view: empty or whitespace-only notes become `None`,
  non-empty notes are stored as-is;
- completed one-shot `run_agent` records now write the final non-empty checkpoint into `SessionRecord` next to
  history/outcome;
- failed/partial one-shot runs also persist the latest checkpoint when a transcript is written;
- interactive `session.*` turns now omit empty checkpoint fields instead of serializing `checkpoint: ""`;
- the working-memory design doc is updated from proposed to implemented, including one-shot persistence coverage.

Tests cover checkpoint snapshots (including whitespace-only omission), compaction-surviving request injection,
session round-trip/staleness behavior, resumed-session checkpoint clearing, completed one-shot persistence, and
partial-run checkpoint persistence.

**Files:** `checkpoint.rs`, `agent/mod.rs`, `session_manager/mod.rs`, `session.rs`,
`docs/designs/agent-working-memory.md`, `README.md`.

---

# Round 29 — workflow definition fail-loud validation hardening

## [x] Item 35 (flow): malformed named workflow refs fail loudly — DONE

Nerve's P3 workflow definitions are now far enough along that the important reliability edge is config
correctness, not merely loading. Leading workflow/agent graph systems such as AutoGen document explicit
validation failures for invalid graph definitions; Nerve should follow the same fail-loud rule for loaded
workflow data.

Shipped behavior:
- `WorkflowRegistry::resolve_if_present` now distinguishes missing workflow refs from present-but-malformed
  workflow definition files;
- flow safety DFS uses that distinction, so a `Named` ref that is also a malformed workflow file returns
  `WorkflowError::NamedWorkflowInvalid` instead of being silently treated as a plain worker leaf;
- every loaded child workflow is now structurally validated and has its own `Named` workers resolved before DFS
  continues, so invalid transitive workflow definitions cannot hide behind a valid parent;
- unknown workflow refs still behave as absent for cycle traversal, preserving the valid worker-leaf case;
- north-star roadmap status was reconciled to current code: P3 workflow defs, P4 MVP containment, and the
  command-executor totality guard are now marked implemented.

Tests cover optional workflow resolution for missing vs malformed files, discovery-precedence edge cases,
`validate_workflow_refs` rejecting a malformed named workflow ref before any worker spawns, and parent workflows
rejecting invalid transitive child workflows with unresolved workers or invalid hierarchy structure.

**Files:** `flow/{workflow_registry,safety}.rs`, `docs/designs/architecture-north-star.md`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 30 — approval UX clarity

## [x] Item 36 (tui): explicit approval action labels — DONE

The GUI/Codex visual-spec checklist calls out approval wording as a safety-critical UI detail: users should
understand whether a decision is one-shot or sticky before they approve an exec/edit action. The Rust TUI kept the
correct key semantics, but the footer text was terse (`[A]lways`) and ambiguous under risk.

Shipped behavior:
- the approval modal footer now renders explicit labels: `[a] Allow`, `[A] Allow for session`, `[d] Deny`,
  `[D] Always deny`, plus `Esc cancel`;
- key handling and protocol decisions are unchanged (`a/y`, `A`, `d/n`, `D`, `Esc` still map to the same
  `SessionApprovalDecision` variants);
- approval modal tests and styled snapshots now pin the clearer labels.

**Files:** `crates/nerve-tui/src/app/render.rs`, `crates/nerve-tui/src/app/snapshots/*approval*.snap`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 31 — GUI approval parity

## [x] Item 37 (gui): expose sticky approval decisions in both web frontends — DONE

Round 30 clarified the Rust TUI approval wording. This round brings the browser surfaces to parity so a user can
make the same one-shot vs. session-sticky decision from the primary Leptos GUI and the legacy single-file fallback.
This follows the Codex-style approval vocabulary already supported by Protocol v4 and the delegate/Codex proxy.

Shipped behavior:
- `crates/nerve-gui` approval modal now labels the sticky allow as **Allow for session** and the sticky deny as
  **Always deny** while preserving the existing `allow_always` / `deny_always` protocol decisions;
- regenerated committed `crates/nerve-gui/dist` so the daemon-served `/app` bundle carries the updated modal;
- legacy `/legacy` `gui.html` now exposes four buttons: Deny, Always deny, Allow, Allow for session;
- legacy approval résumé chips distinguish `allowed for session` and `always denied` from one-shot decisions;
- daemon embedding tests now assert both the Leptos wasm bundle and the legacy HTML expose the full sticky approval
  vocabulary.

**Files:** `crates/nerve-gui/{src/approval.rs,dist/*}`, `crates/nerve-workstation/src/daemon/{app.rs,gui.html,http.rs}`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 32 — Codex-style composer mode shell

## [x] Item 38 (gui): composer Local / Worktree / Cloud segmented affordance — DONE

The visual-spec P0 checklist calls the composer mode selector the most recognizable Codex surface. Nerve already
had the functional delegate composer, autonomy picker, model picker, and send/stop behavior; this round reshapes the
primary Leptos GUI composer into the Codex-style two-row card without changing protocol semantics.

Shipped behavior:
- the primary `/app` composer now has a top toolbar with a **Local / Worktree / Cloud** segmented mode shell;
- **Local** is the active execution mode, while **Worktree** and **Cloud** are deliberately disabled and labeled
  coming-soon so the UI does not imply unavailable execution semantics;
- attach and dictate affordance buttons live on the same top toolbar and are also disabled until backed by real
  behavior;
- the input row keeps the existing autonomy picker, model picker, and Send/Stop controls unchanged;
- composer styling now matches the visual spec more closely: 12px card radius, soft shadow, focus border, segmented
  pill styling, responsive column stacking, and the Codex placeholder `Describe a task…  /  for commands`;
- daemon embedding tests pin both source-level mode declarations and the committed wasm bundle strings so `/app`
  cannot drift from source silently.

**Files:** `crates/nerve-gui/{src/app.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 33 — Codex-style topbar model picker

## [x] Item 39 (gui): topbar model-picker pill + command chrome — DONE

Round 32 added the Codex-style composer execution-mode shell. This round removes the remaining raw topbar agent
select from the primary Leptos GUI and turns it into a compact model-picker pill with a popover, matching the
visual-spec direction while preserving the exact delegate configuration signals.

Shipped behavior:
- the topbar now renders a **model-picker pill** summarizing `agent · model` instead of a raw select;
- opening the pill shows a popover with the existing Agent and Model selects, still wired to the same persisted
  `agent` / `model` signals used by `delegate.start`;
- terminal (`⌘J`) and pop-out (`↗`) icon buttons are present as disabled coming-soon affordances, while the existing
  Context and Task pane controls remain functional;
- `data::model_label` centralizes display labels for the selected per-agent model id;
- source and embedded-bundle guards assert the `/app` UI exposes the model picker and command-chrome affordances;
- the visual-spec checklist now marks the topbar model-picker P0 item complete.

**Files:** `crates/nerve-gui/{src/app.rs,src/data.rs,src/topbar.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 34 — Codex-style empty state alignment

## [x] Item 40 (gui): task-first empty/home state — DONE

Codex-style agent workspaces open on a task-first home surface: a calm prompt, the composer as the focal element,
and a few light suggestion chips. Nerve's primary GUI already dropped the old logo splash in earlier GUI work, but
its copy and suggestions still diverged from the visual spec.

Shipped behavior:
- the empty thread hero now says **What should we work on?**;
- a muted subtitle shows the active workspace/project name under the greeting, with a fallback while workspaces load;
- suggestion chips are now the spec-aligned **Plan**, **Ask**, and **Explain this repo** actions, each pre-filling
  the composer with a concrete prompt and returning focus to the composer;
- hero typography now matches the 26px/600 spec with a small project subtitle;
- source and embedded-bundle guards pin the empty-state copy, suggestion group semantics, focus wiring, and committed `/app` wasm strings.

**Files:** `crates/nerve-gui/{Cargo.toml,src/app.rs,src/data.rs,src/dom.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 35 — Codex visual token baseline

## [x] Item 41 (gui): finish GUI P0 token and section-label alignment — DONE

The remaining visual-spec P0 work was global polish rather than protocol behavior: align the primary GUI token
baseline with the Codex-style neutral surface and make sidebar section labels read like source-list chrome.

Shipped behavior:
- light-theme tokens now match the spec baseline more closely: brighter `--bg`, softer `--surface-2`, Codex-style
  sidebar/inspector surfaces, hairline borders, near-black monochrome accent, and explicit shared radii variables;
- the reading column and sidebar width now use the spec metrics (`--col: 720px`, `--sidebar-width: 260px`);
- `--font-ui`, `--font-code`, `--topbar-h`, and `--fs-label` are declared as first-class theme knobs, and touched code/pre/chrome areas begin consuming them;
- dark/system theme branches now carry the matching `--inspector` token;
- body chrome keeps compact line-height while transcript content keeps a relaxed reading rhythm;
- sidebar section labels are uppercase, tracked (`0.04em`), and font-sized via `--fs-label`;
- the conversation section is renamed to **Threads**, matching the visual spec wording;
- source, embedded CSS, and embedded wasm guards pin the token/label changes and committed `/app` sidebar text.

**Files:** `crates/nerve-gui/{src/sidebar.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 36 — Codex sidebar structure and icons

## [x] Item 42 (gui): finish GUI P1 sidebar hierarchy — DONE

This round moves the `/app` sidebar from a functional conversation list toward the Codex source-list hierarchy without changing runtime protocol behavior.

Shipped behavior:
- the sidebar now has a dedicated **Workspace navigation** group ordered as **Threads**, **Chats**, **Automations**, and **Skills**;
- letter/symbol glyphs are replaced by local inline SVG icons (`.nav-svg`) styled with current-color strokes, keeping the bundle self-contained and avoiding third-party/proprietary assets;
- **Chats** is present as a disabled/coming-soon affordance so the IA matches the visual spec while preserving current behavior;
- the thread rail is visually nested under the active project with a subtle left guide and `rail-nested` wrapper;
- thread rows now use a two-line layout: title plus relative-time metadata (`rail-sub`), matching the spec's sidebar secondary-line treatment;
- source, embedded CSS, and embedded wasm guards pin the navigation label, **Chats** text, SVG/icon CSS, nested rail wrapper, and committed `/app` bundle strings.

**Files:** `crates/nerve-gui/{src/sidebar.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 37 — Codex transcript and tool-card polish

## [x] Item 43 (gui): finish GUI P1 transcript/tool-card polish — DONE

This round completes the remaining visual-spec P1 transcript polish while preserving the existing delegate/tool event model.

Shipped behavior:
- transcript turn spacing now matches the Codex-style 28px rhythm;
- user bubbles are borderless `--surface-2` cards with the shared `--r-card` radius;
- tool cards use the shared card radius and render a small status dot instead of a word badge;
- running tool dots pulse in amber, successful tools are green, and failed tools are red, reusing the existing `ToolCard.ok` state;
- tool names now consume `--font-code` instead of a hard-coded monospace stack;
- reasoning rows now use quieter **Thought for this step** wording;
- source, embedded CSS, and embedded wasm guards pin the dot markup, transcript spacing, font token use, and committed `/app` strings.

**Files:** `crates/nerve-gui/{src/render.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 38 — Inspector closure audit

## [x] Item 44 (gui): close the existing inspector P2 checklist item — DONE

Audit found the right-pane shell was already implemented in the current GUI surface: the shell carries `class:with-inspector`, the topbar has a Task pane toggle, and the pane exposes Plan/Files/Changes tabs.

Shipped behavior/evidence:
- `#nerve-shell.with-inspector` switches to the three-column grid with `--inspector-width`;
- `<aside class="inspector">` renders as a collapsible sibling of `<main>`;
- the **Plan** tab renders the active turn's tool calls as `plan-step` rows with an empty-state message;
- **Files** and **Changes** tabs load real snapshot-backed data via `fetch_file_tree` and `fetch_diff`;
- responsive CSS drops the inspector on narrow viewports;
- source, embedded CSS, and embedded wasm guards now pin the inspector shell, tabs, plan empty state, and committed `/app` strings.

**Files:** `crates/nerve-workstation/src/daemon/app.rs`, `docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 39 — Codex theme hook

## [x] Item 45 (gui): formalize theme overrides — DONE

This round turns the existing base-theme picker into a Codex-style theme hook: base theme remains system/light/dark, while optional token overrides can tune the app without new runtime protocol vocabulary.

Shipped behavior:
- persisted settings now include optional `accent`, `bg`, `fg`, `font_ui`, and `font_code` fields;
- `apply_theme` keeps driving `data-theme` for system/light/dark and additionally maps non-empty fields to `--accent`, `--bg`, `--fg`, `--font-ui`, and `--font-code` on `<html>`;
- empty override fields remove the inline CSS variable and fall back to the monochrome Codex default tokens;
- the settings modal exposes Accent, Background, Foreground, UI font, and Code font inputs with quiet `set-input` styling;
- the pre-paint `index.html` script applies the same overrides from `localStorage` before WASM loads to avoid theme flash;
- source, embedded HTML/CSS, and embedded WASM guards pin the persisted keys, CSS variable mapping, settings labels, and committed `/app` bundle.

**Files:** `crates/nerve-gui/{Cargo.toml,index.html,src/app.rs,src/settings.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 40 — Sidebar vibrancy capability

## [x] Item 46 (gui): add optional macOS-style sidebar vibrancy flag — DONE

This round completes the final GUI visual-spec checklist item without changing the default appearance.

Shipped behavior:
- the sidebar keeps its normal opaque `--sidebar` background by default;
- when `<html data-vibrancy="sidebar">` is present and the browser supports backdrop filtering, the sidebar switches to a translucent material-like fill;
- the vibrancy rule is gated behind `@supports` and uses both `backdrop-filter` and `-webkit-backdrop-filter` for macOS/WebKit shells;
- the pre-paint script reads the persisted `sidebar_vibrancy` capability flag and applies/removes `data-vibrancy` before WASM loads;
- source, embedded HTML/CSS, and committed `/app` guards pin the capability flag and CSS rule.

**Files:** `crates/nerve-gui/{index.html,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/gui-codex-visual-spec.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 41 — Vibrancy setting closure

## [x] Item 47 (gui): expose the sidebar vibrancy capability in Settings — DONE

Round 40 added the capability hook; this round makes it user-controllable through the same persisted Settings path as the Codex-style theme knobs.

Shipped behavior:
- `Settings` now persists `sidebar_vibrancy: bool` next to the other appearance settings;
- `apply_theme` applies or removes `data-vibrancy="sidebar"` during normal WASM runtime updates, not just pre-paint;
- Settings modal includes a **Sidebar material** row with a **Vibrant sidebar** toggle;
- `set-toggle` styling matches the quiet settings controls and keeps the default off;
- source, CSS, and embedded WASM guards pin the setting field, toggle label, and committed `/app` bundle strings.

**Files:** `crates/nerve-gui/{src/app.rs,src/settings.rs,styles.css,dist/*}`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 42 — Provider registry named API closure

## [x] Item 48 (provider): finish the config-driven provider registry API — DONE

This round closes the last P2 polish item in the architecture north star: provider config was already
runtime-resolvable, but UI/agent-definition code still lacked a deterministic named registry surface for
validation and discovery without creating credentials or network clients.

Shipped behavior:
- `ProviderWire::as_str` exposes stable wire names for registry metadata;
- `ProviderSource` and `ProviderDescriptor` describe built-in and config-defined selectable providers;
- `ProviderRegistry::descriptors()` lists built-ins first, then config providers in sorted config-name order;
- `ProviderRegistry::descriptor(name)` resolves one built-in name/alias or config provider name without reading credentials;
- `ProviderRegistry::contains_name(name)` now delegates to the descriptor lookup;
- unknown-provider diagnostics list the known provider registry entries with source, provider id, wire, aliases, base URL, and API key environment variable;
- config entry names are normalized before insertion, so descriptor output and duplicate detection use the trimmed selectable name;
- provider tests cover descriptor ordering, named alias/config lookup, unknown-provider diagnostics, and existing explicit-key/env resolution.

**Files:** `crates/nerve-workstation/src/providers.rs`, `docs/designs/architecture-north-star.md`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 43 — Auth broker access-token lease MVP

## [x] Item 49 (auth): add the host-managed OAuth lease seam — DONE

This round starts the Auth broker roadmap item without violating the north-star OAuth topology: the daemon
still does not capture loopback callbacks, and the trusted node remains the holder of the refresh token.
Instead, clients can ask the host for OAuth lease metadata through runtime protocol; Round 47 later hardened
runtime lease jobs so bearer tokens are not returned over this surface.

Shipped behavior:
- added additive runtime command `auth.lease { provider, force_refresh? }` to `RuntimeCommand` and `RUNTIME_COMMAND_NAMES`;
- `Runtime` still refuses `auth.lease` at the core hub, preserving host-only auth execution;
- `jobs.rs::executor_for` routes `auth.lease` to the host `AuthManager`, and executor partition tests have a representative command;
- `AuthManager::lease` loads the stored credential, refreshes via `nerve_agent::auth::ensure_fresh` when needed/requested, and returns a broker lease payload;
- lease payload includes provider/status/mode/base URL/account/expires/access token metadata, but never includes `refresh_token`;
- API-key credentials are refused for leasing because they are long-lived secrets, not broker-managed OAuth tokens;
- protocol schema artifacts were regenerated and drift-checked.

**Files:** `crates/nerve-proto/src/command.rs`, `crates/nerve-runtime/src/{lib.rs,runtime.rs}`,
`crates/nerve-workstation/src/{auth/manager.rs,jobs.rs}`, `docs/protocol/runtime-v3.schema.json`,
`docs/designs/architecture-north-star.md`, `docs/plans/borrow-impl-plan.md`.

---

# Round 44 — TUI Auth broker lease UX

## [x] Item 50 (tui/auth): consume `auth.lease` from the native client — DONE

Round 43 added the host-managed broker seam. This round proves a native runtime-protocol client can
consume it safely: the TUI requests an OAuth lease from the daemon but renders only redacted metadata.

Shipped behavior:
- added `/lease [provider] [--refresh]` to the TUI slash-command palette and help;
- the command defaults to the current provider and maps `--refresh` / `--force-refresh` to `AuthLease.force_refresh`, with help text warning that refresh forces broker refresh;
- `cmd_lease` calls `NerveClient::run_job(RuntimeCommand::AuthLease { ... })`, so it uses the real daemon job protocol;
- lease output displays provider/status/base/account/expiry and whether the refresh token is held by the broker;
- access tokens are explicitly redacted in the transcript, and tests assert the raw token plus secret-shaped field names (`Bearer`, `access_token`, `refresh_token`, JSON debug output) do not appear;
- docs now record that the Rust TUI consumes the broker lease while web/mobile UX remains future work.

**Files:** `crates/nerve-tui/src/app/input/{mod.rs,auth.rs}`, `crates/nerve-tui/src/ui/commands.rs`,
`docs/designs/{architecture-north-star.md,agent-config-and-model-selection.md}`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 45 — Web GUI Auth broker lease UX

## [x] Item 51 (gui/auth): consume `auth.lease` from Settings — DONE

Round 44 proved the native TUI can consume broker leases safely. This round brings the same seam to
the primary Leptos Web GUI without moving bearer credentials into the browser: Settings now requests
`auth.lease` through the existing `/rpc` job protocol with `include_token=false` and renders metadata only.

Shipped behavior:
- protocol `auth.lease` gained `include_token` (default true for existing native clients; false for browser metadata-only calls);
- Settings includes a **Broker OAuth lease** section with Claude / ChatGPT(OpenAI) / xAI provider choices;
- **Check lease** calls `auth.lease` with `include_token=false`, while **Force refresh lease** also sets `force_refresh=true` and labels the action as forcing broker refresh/rotation;
- output shows provider/status/base URL/account/expiry plus "not returned to Web GUI" access-token status and never-returned refresh-token wording;
- redaction/error tests assert raw token text and secret-shaped field names (`Bearer`, `access_token`, `refresh_token`, JSON debug output) do not appear;
- CSS adds a compact `.lease-status` live-region metadata panel inside the settings modal;
- daemon `/app` source/CSS/WASM guards pin the source strings, committed CSS, and rebuilt committed WASM bundle.

**Files:** `crates/nerve-proto/src/command.rs`, `crates/nerve-runtime/src/lib.rs`,
`crates/nerve-workstation/src/{auth/manager.rs,jobs.rs,daemon/app.rs}`,
`crates/nerve-tui/src/app/input/auth.rs`, `crates/nerve-gui/{src/app.rs,src/settings.rs,styles.css,dist/*}`,
`docs/protocol/runtime-v3.*.json`, `docs/designs/architecture-north-star.md`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 46 — Auth device-code protocol seam

## [x] Item 52 (auth/mobile): reserve device-code login flow without weakening browser login — DONE

Round 45 made Web GUI lease checks metadata-only. This round prepares the remaining mobile/remote auth
fallback without pretending provider device-code endpoints are already implemented: `auth.start` now has
an explicit `flow` field, browser login remains the default wire shape, and device-code requests fail
closed with a clear host error instead of silently falling back to browser OAuth.

Shipped behavior:
- `RuntimeCommand::AuthStart` gained `flow: AuthStartFlow` with default `browser` serialization omitted for wire compatibility;
- `RuntimeCommand::auth_start(...)` and `auth_start_with_flow(...)` helper constructors reduce Rust API migration friction;
- `flow=device_code` round-trips through the protocol/schema and is re-exported by `nerve-runtime`;
- `AuthManager` returns browser start payloads with `flow="browser"` and rejects device-code starts before allocating pending login state or emitting auth events;
- TUI `/login [provider] [--device]` now documents that device-code login is reserved/fail-closed until provider endpoints are wired;
- representative job payloads, runtime protocol export, and roadmap docs are updated.

**Files:** `crates/nerve-proto/src/{command.rs,lib.rs}`, `crates/nerve-runtime/src/lib.rs`,
`crates/nerve-workstation/src/{auth/manager.rs,jobs.rs}`, `crates/nerve-tui/src/{app/input/mod.rs,ui/commands.rs}`,
`docs/protocol/runtime-v3.*.json`, `docs/designs/{architecture-north-star.md,agent-config-and-model-selection.md}`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 47 — Auth capability discovery

## [x] Item 53 (auth/mobile): expose provider auth capabilities through `auth.status` — DONE

Round 46 made `device_code` an honest fail-closed protocol value. This round makes that state discoverable
without forcing clients to probe failed starts: `auth.status` now returns a stable `capabilities` object for
both authenticated and not-logged-in providers.

Shipped behavior:
- `auth.status` and completed credential status include `capabilities.auth_start.browser.supported=true`;
- `capabilities.auth_start.device_code.supported=false` carries an explicit reason until provider device endpoints are wired;
- `capabilities.auth_lease` advertises metadata availability, explicit runtime non-support for bearer-token output, and no stored refresh-token return;
- logged-out/logout status payloads include the same capability matrix, still without any credential material;
- tests recursively assert capabilities/status/lease payloads do not expose exact `access_token` or `refresh_token` fields, and docs point clients at status discovery instead of trial-and-error.

**Files:** `crates/nerve-workstation/src/auth/manager.rs`,
`docs/designs/{architecture-north-star.md,agent-config-and-model-selection.md}`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 48 — Web GUI auth capability discovery

## [x] Item 54 (gui/auth): consume `auth.status.capabilities` in Settings — DONE

Round 47 made auth capabilities discoverable over the runtime protocol. This round makes the primary
Leptos Web GUI consume that discovery data instead of leaving it as invisible protocol metadata.

Shipped behavior:
- Settings' broker section is now **Broker OAuth** with one provider picker for status and lease actions;
- **Check status** calls `auth.status` through the existing `/rpc` job path and renders provider/status/mode;
- the status panel shows browser-login support, device-code unsupported reason, lease metadata availability,
  runtime bearer-token non-exposure, and stored-refresh non-return;
- daemon-controlled status/reason text is redacted before rendering if it contains secret-shaped fragments;
- if a future trusted-native bearer capability exists, the Web GUI still labels it as not requested by Web GUI;
- the provider picker is disabled while status/lease jobs are in flight so stale results are harder to misread;
- status formatting tests reject secret-shaped output (`Bearer`, `access_token`, `refresh_token`, JSON debug text);
- `.auth-status` styling and daemon source/CSS/WASM guards pin the new UI, formatter wiring, and rebuilt `/app` bundle.

**Files:** `crates/nerve-gui/{src/settings.rs,styles.css,dist/*}`,
`crates/nerve-workstation/src/daemon/app.rs`, `docs/designs/architecture-north-star.md`,
`docs/plans/borrow-impl-plan.md`.

---

# Round 49 — Web GUI staged browser OAuth

## [x] Item 55 (gui/auth): start/complete browser OAuth without exposing tokens — DONE

The Web GUI could inspect auth capabilities and broker lease metadata, but still had to tell users to
leave the GUI for provider login. This round adds a safe staged browser-login path over the existing
runtime `auth.start` / `auth.complete` seam.

Shipped behavior:
- Broker OAuth controls moved out of `settings.rs` into `settings_auth.rs`, keeping Settings small and
  leaving auth-specific formatting/tests near the UI that uses them;
- **Start browser login** calls `auth.start` with `flow="browser"`, stores only `login_id` and
  `authorize_url` in UI state, and renders a same-page external link to open the provider authorize URL;
- **Complete login** submits a pasted callback URL or bare code to `auth.complete`; token exchange,
  refresh-token persistence, and credential storage remain inside the daemon auth manager;
- completion/status/lease formatters intentionally render metadata-only summaries and tests reject
  `access_token`, `refresh_token`, `Bearer`, raw JSON, and fixture secret values;
- daemon embedded-app guards now include `settings_auth.rs`, auth.start/auth.complete wiring,
  callback/code paste UI, login-status CSS, and rebuilt `/app` WASM strings.

**Files:** `crates/nerve-gui/src/{settings.rs,settings_auth.rs}`, `crates/nerve-gui/styles.css`,
`crates/nerve-gui/dist/*`, `crates/nerve-workstation/src/daemon/app.rs`,
`docs/designs/architecture-north-star.md`, `docs/plans/borrow-impl-plan.md`.
