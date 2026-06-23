# CodeGraph — a deterministic, persistent cross-file code-intelligence engine

Status: **in progress** — shipped on branch `feat/deterministic-kernel` (all CI gates green):
PR0 (deterministic kernel), PR1 (shared snapshot-memoized index), PR1b (memoized derived
ReferenceGraph), PR1c (shared inverted definition index; `find_references` rerouted), a generic
`SnapshotMemo<T>` consolidating the three T0 caches, and `detect_changes` (unified diff → touched symbols)
and `trace_path` (shortest call chain between two symbols, BFS composing
`call_hierarchy`) — the first two new agent-facing tools, the build-side of the PR5 query
surface brought forward as pure deterministic core tools. PR2 (confidence-tiered resolver) next.
Governs a structural change — read `docs/designs/architecture-north-star.md` first.
Date: 2026-06-23
Related: `architecture-north-star.md` (determinism boundary, seam table, P7 cockpit),
`agent-long-term-memory.md` (the *agent-fact* memory — a different subsystem; see §8).

## 1. Problem & ambition

The headline direction (2026-06-23) is a **cockpit orchestrating external CLI coding
agents** (Claude Code, Codex, Gemini) that are handed Nerve's engine as MCP tools. The
engine is therefore the product's moat, and the bar is **absolute industry leadership** in
agent-facing code intelligence: best-in-class on the axes that matter to a delegated agent —
**determinism, token-efficiency, latency, and large-repo scale**.

Today the lexical/structural tools are excellent and deterministic, but each
navigation/repo-map call **re-derives the whole cross-file index from scratch**:
`goto_definition` / `find_references` / `call_hierarchy` / `analyze_impact` /
`find_referencing_symbols` / `get_repo_map` / `build_context` each call
`indexed_files_cancellable` (`repomap/analysis.rs`) — O(repo) per call. The `(mtime,size)`
parse cache (`catalog/mod.rs`) saves re-*parsing*, not re-*derivation*, and there is **no
higher-level memo and no persistent cross-file graph**. `build_context` even walks the repo
**twice** (`build_context/mod.rs:127` + `reference_expansion.rs:41`).

The reference project `codebase-memory-mcp` (C + SQLite) shows what "leading" looks like on
the perf axis (incremental re-index, sub-ms structural queries, Linux-kernel scale) — but its
mutable-SQLite-everything model is the wrong shape for Nerve. We take its **algorithm shapes
as inspiration only** (it is not in this repo; no constant is imported as fact) and build the
deterministic spine ourselves, while **consuming** its non-deterministic breadth via MCP.

## 2. Decision

**CodeGraph**: one shared, deterministic cross-file relationship graph computed as a **pure
function** of the `CatalogSnapshot` inside `nerve-core`, golden-tested like every other tool;
three derived-cache tiers; non-deterministic edge families consumed via the existing
MCP-client seam, never built into the kernel.

The build-vs-consume line **is the determinism boundary itself**:
- **BUILD in `nerve-core`** (the moat): symbol / definition / reference / import / call edges
  + confidence — pure, re-computable, golden-testable.
- **CONSUME via MCP** (`McpClientToolAdapter`, above the kernel): every non-deterministic /
  peripheral family — community/architecture clustering, model-free or embedding similarity,
  git co-change coupling, route/cross-service rendezvous, infra/k8s scanning — and
  codebase-memory-mcp's whole property-graph + Cypher engine, tagged `deterministic:false`.

### 2.1 Prerequisite (Phase 0, in flight): a fully-deterministic kernel
The heavyweight ONNX `semantic` feature (fastembed embeddings + hnsw ANN + cross-encoder
rerank) is the **one** thing breaking nerve-core's purity (model-dependent, approximate,
network-downloading, excluded from the default golden suite) and it taxes exactly the hot
files CodeGraph touches. It is being **removed** (branch `feat/deterministic-kernel`). The
*concept-search capability* is consciously deferred and re-enters later through the right door
(a deterministic in-core algorithmic tier, or a consumed MCP server) — decided at the
Phase-1 measurement gate (§6). After removal, `nerve-core` is 100% deterministic and
golden-testable.

## 3. The three tiers (every tier is a derived cache, never authoritative)

| Tier | What | Where | Gate |
|---|---|---|---|
| **T0** | always-on in-process memo holding `Arc<CodeGraph>` | `nerve-core`, default build | always on; pure |
| **T1** | content-addressed on-disk graph (cold-load + incremental) | `nerve-core`, `graph-cache` feature (off by default) | mirrors the old `semantic` gating |
| **T2** | concept/meaning recall | deferred — deterministic in-core algo **or** consumed MCP | Phase-1 decision |

### 3.1 The memo key (the correctness crux — verified)
T0 must **not** key on `snapshot.generation`: `FsCatalogProvider` hard-codes
`CatalogSnapshot.generation = 1` permanently (`catalog/fs_scan.rs:186`); the real edit counter
is a *separate* `ProviderCache.generation: AtomicU64` (`catalog/mod.rs:93`, bumped at
`:197`/`:209`) that never reaches the snapshot value tools observe. A generation-keyed memo
returns a **stale graph after an edit** (hit != miss) — a determinism violation.

Correct key = the same dual key the (now-removed) semantic index already proved safe:
- **primary**: `Arc::ptr_eq` against the snapshot `Arc` from `provider.snapshot_arc()`
  (`port.rs:33`) — O(1), correct on both providers within the snapshot-cache TTL; the provider
  drops the cached `Arc` to `None` on every write/delete/rename (`catalog/mod.rs:~208`), so an
  edit forces a fresh `Arc`;
- **fallback**: an `(mtime,size)` content fingerprint (via `port.rs:61` `file_signature`,
  FNV from `summary_cache.rs:118`, **not** sha2) so an identical-bytes rescan with a new `Arc`
  still hits.

**Falsifiable gate:** a `hit == miss` regression test that runs on **`FsCatalogProvider`**
specifically (edit a file → invalidate → rescan → assert the served graph reflects the edit,
not the stale memo). MemoryCatalogProvider-only tests would hide a generation-key bug because
its generation *is* monotonic (`memory.rs:140`).

## 4. Data model

Stable identity first (today files are keyed by snapshot `Vec` position):
- `FileKey = (root_id, rel_path)`; `Qn` (qualified name) `= (root_id, rel_path,
  enclosing-symbol path)`. `Qn` is the in-memory cross-pass join key; its use as a **durable
  persisted** key (T1/PR4) is gated on a churn proof (overloaded/nested/anonymous symbols),
  adding a deterministic intra-file ordinal from the snapshot-stable traversal order if
  enclosing-path is non-unique. Identity never bakes in line/column.
- In memory: `CodeGraph { nodes: Vec<GraphNode>, by_qn, by_name: BTreeMap<(LangFamily,Name),
  Vec<NodeId>>, out_edges/in_edges: Vec<Vec<EdgeRef>>, + the f64 PageRank weights kept
  alongside banded confidence so `get_repo_map` PageRank stays byte-identical }`.
- `GraphEdge { src, dst, kind: {References, Import(weight), Calls, DefinedBy}, weight: f64,
  confidence: ResolveBand{High,Medium,Speculative}, strategy }`. The existing 2-way
  `Confidence{High,Low}` stays the **public contract**; bands are an *additive* refinement so
  existing goldens stay byte-stable.
- On disk (T1 only): `PersistedGraphManifest` mirroring the old semantic `PersistedManifest`
  (schema/resolver versions, `workspace_key` fingerprint, per-file signatures + `node_qns` /
  `edge_ids`, node/edge **binary** sidecars — not inline JSON).

### 4.1 Confidence banding (determinism hazard — resolved by design)
Banding is a **pure function of the snapshot-complete candidate set**: the candidate count
that drives any count-based penalty is computed over the full snapshot, so an edge's band is
identical for cold and incremental builds. Enforced by an `incremental == cold` band-stability
golden that **perturbs an unrelated file** and asserts an unchanged edge's band is stable. The
resolver chain (first-hit-wins: import-map / same-module / qualified-suffix / unique-name /
suffix-match, candidate-count penalty, hard unresolvable cutoff, final lexical tiebreak on
`Qn`) has every constant **re-derived against Nerve's own insta fixtures** and pinned by
goldens + a `RESOLVER_VERSION` const. codebase-memory-mcp supplies **no** constant.

## 5. Seams (north-star compliant)

- `CatalogProvider` (`port.rs`) — **unchanged** for T0 (uses only `snapshot_arc()` +
  `file_signature()`). T1 persistence reached via an optional
  `#[cfg(feature="graph-cache")]` accessor mirroring the old `semantic_index()` hook.
- Single dispatch hub (`dispatch/mod.rs`) — nav/repomap tools already route here; CodeGraph
  sits *below* them. A later read-only `code_graph_query` / `trace_path` registers as one more
  pure tool via `dispatch/specs.rs` + `handlers.rs`.
- New `graph-cache` cargo feature — the sanctioned mechanism for persistent/heavyweight
  derived state; default golden build stays pure. CI gains a job running clippy+golden under
  `--features graph-cache`.
- `McpClientToolAdapter` (`crates/nerve-workstation/src/mcp/adapter.rs`) + a normalizing
  `RuntimeToolAdapter` — the **consume** seam for non-deterministic edge families, gated on
  PolicyToolBox/SandboxLauncher coverage for the third-party server.

## 6. Phasing (each phase gated; stop early if measurement says so)

- **PR0 — SHIPPED (`4e2aa58`)** — removed the ONNX semantic stack → fully-deterministic
  kernel; all gates green, runtime protocol unchanged.
- **PR1 — SHIPPED (`a696c6a`)** — `crates/nerve-core/src/graph/{mod.rs, memo.rs}`: a
  process-global, snapshot-**Arc-identity**-memoized shared `Vec<IndexedFile>`
  (`shared_indexed_files`), keyed on `Weak<CatalogSnapshot>` + `Arc::ptr_eq` (NOT
  `snapshot.generation` — frozen at 1 on FS). The 7 navigate entry fns + `build_context` +
  `reference_expansion` + `symbol_rename` reroute off per-call `indexed_files_cancellable` to
  the one shared index; output **byte-identical** (zero golden diffs — verified). Ships the
  FsCatalogProvider `hit==miss` memo test + a ptr_eq hit-path test + a direct-parity test.
  No disk, no feature, no new tool. `get_repo_map` left on `analyze_files_cancellable`.
- **PR1b — SHIPPED (`32c5a9c`)** — `graph/derived.rs` `shared_reference_graph`: a second
  snapshot-Arc-memoized cache for the derived `ReferenceGraph` (the per-call O(edges)
  cross-file graph build), so `get_repo_map` + `build_context` reuse one graph while the
  provider serves the same snapshot Arc. Byte-identical by construction (`ReferenceGraph::build`
  is a pure fn of `&[IndexedFile]`, ignores `query_match`); zero golden diffs; FsCatalogProvider
  `hit==miss` + ptr_eq tests.
- **PR1c — SHIPPED (`41a7272`)** — `graph/definitions.rs` `shared_definition_index`: a third
  snapshot-Arc memo holding an unpruned inverted index `name → [file idx per defining symbol
  occurrence]`. `find_references` (`definition_file_indexes` + `count_definitions`) now does
  O(occurrences) lookups instead of two O(repo·symbols) scans, reused across calls on the same
  snapshot. Byte-identical (references' predicate is exactly `symbol.name == name` + the
  unchanged per-file `language_matches`); zero golden diffs; `hit==miss` + ptr_eq + count-parity
  tests. **De-risk finding:** only `find_references` shares the plain name+language predicate;
  `find_referencing_symbols`/`analyze_impact` add kind+path filters and repomap's
  `definition_index` is HDF-pruned, so those are NOT byte-identically unifiable and keep their
  own derivation — this index can later *pre-filter* their scans by name (candidates) as a
  separate change. *Still deferred:* collapsing the `build_context` repo-map/indexed_files
  double walk (needs unifying `analyze_files` + `indexed_files`).
- **PR2** — `graph/resolver.rs`: confidence-tiered resolution; constants re-derived + golden
  + `RESOLVER_VERSION`; public `Confidence{High,Low}` preserved, bands additive; the
  `incremental==cold` band-stability fixture lands here.
- **Phase-1 measurement gate (before PR3)** — instrument T0 hit-rate + real fast-path cost,
  per-call nav/build_context latency on a large repo, cold-build latency, resident RAM (one
  large repo **and** many-workspaces-in-one-daemon), daemon-restart cost, memo-lock contention
  under N concurrent agents, and whether the cockpit's bottleneck is nav latency or
  GUI/observability. **A negative result is explicit licence to stop after PR1–PR2.** Also
  decides the T2 (concept-search) build-vs-consume question.
- **PR3 (gated)** — first extract `crate::persist` out of the (removed) semantic island's
  shape into a shared module (its own PR), then add `graph-cache` + `graph/persist.rs`:
  cold-load content-addressed graph, fail-closed; new CI job; `cached == cold` golden.
- **PR4 (gated on Qn-stability proof)** — `graph/incremental.rs`: signature-diff reconcile
  (recompute changed files only) with inbound cross-file edge snapshot-by-endpoint-`Qn`-before-
  purge + re-link-after-resolve; `incremental == cold` parity for edges **and** bands.
- **PR5 (gated on permission/sandbox coverage)** — read-only `code_graph_query` / `trace_path`
  (bounded BFS) + the consume facade (`graph_query`/`find_related`/`architecture_map`/
  `trace_path` → `mcp__memory__*`, `deterministic:false` provenance; trust-ranking enforced in
  the cockpit, not the kernel).
- **Stretch (only if measured)** — a model-free, deterministic `SIMILAR_TO` family
  (MinHash/LSH) behind `graph-cache`, complementing (never duplicating) any T2 concept search.

## 7. Determinism story (where impurity lives)

Default build gains **zero** new non-determinism. `CodeGraph::build` is a pure function of
`(provider, snapshot)` (same bytes → byte-identical graph), pinned by insta goldens with the
existing deterministic ordering (`BTreeMap`, `reference_cmp`/`symbol_cmp`, `{:.8}` PageRank
formatting). T0 is pure memoization (hit==miss, bounded LRU, FNV not sha2). T1 is
content-addressed + fail-closed (discard-and-rebuild on any schema/resolver/checksum mismatch,
clock injected) — provably reconstructable, hence never authoritative. Consumed MCP edges are
tagged `deterministic:false` and live above the kernel. The hardest guarantee —
`incremental output == cold output` — is an enforced parity golden.

## 8. Divergence from `agent-long-term-memory.md` (intentional, scoped)

That doc frames **all** memory as agent-curated non-deterministic state that must live in
`nerve-workstation` behind `MemoryStore` (file-first, SQLite deferred). That is correct **for
agent facts** and unchanged. A cross-file code graph is a different category — a **derived,
content-addressed index re-computable from the snapshot** — so it legitimately lives in
`nerve-core` as a pure cache (the `summary_cache.rs` in-memory + former
`semantic/persistence.rs` on-disk precedents prove core already does exactly this). The "first
DB dependency" objection is *sidestepped*: T1 is a flat-file + content-hash + atomic-write
artifact (binary sidecars), **not** SQLite; codebase-memory-mcp's SQLite is consumed via MCP,
never embedded. The "store pointers, not contents" rule is narrowly relaxed (T1 materializes
derived edges) while preserving its intent via content-addressing + fail-close, so stale state
can never alter golden output.

## 9. No HARD invariant is relaxed

Default build stays pure (T0 in-memory, snapshot-Arc + fingerprint key); disk/clock behind the
off-by-default `graph-cache` feature; single dispatch hub preserved (tools reroute internally,
no new entry point); runtime protocol untouched (the graph is core-internal vocabulary, not
protocol vocabulary); `file ≤600` / `fn ≤100` / `nesting ≤6` honored via module-dir splits.
