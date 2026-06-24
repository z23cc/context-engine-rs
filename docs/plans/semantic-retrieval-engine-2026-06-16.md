# Self-Contained Semantic + Hybrid Retrieval Engine: Plan

> **Superseded / historical (2026-06-24):** the in-kernel semantic/embedding (ONNX) retrieval engine planned here was subsequently removed (PR0 ONNX removal), and INV-R2 now forbids learned models in `nerve-core`; semantic recall, if any, is MCP-consumed and tagged `deterministic:false` — see `docs/designs/trust-substrate.md` (INV-R2) and `docs/designs/code-graph.md`. Kept as a record; do not treat as current direction.

## Goal

Add an Augment-style semantic retrieval layer to Nerve Workstation — local code embeddings
+ ANN + cross-encoder rerank, fused with the existing BM25 — so an agent gets
intent-based ("where is the code that does X") recall, not just lexical/structural
matches. Open-source components only; effect (retrieval quality) is the priority.
Self-contained (data stays local, no required external service), with a pluggable
embedding backend so a BYO/hosted endpoint stays possible.

## Working assumptions (user skipped the clarifying questions — overridable)

- **Build self-contained** with open-source components; pluggable backend leaves
  a hosted/external option open.
- **Persistent on-disk index — promoted to v1** (decision 2026-06-16). The
  in-memory MVP (Work Items 1–8, done) proved retrieval quality, but a real-model
  eval measured a **~4.6 min cold first query** just to embed 841 chunks of
  `nerve-core/src` (CPU, fp32 `jina-v2-base-code`) — rebuild-per-session is not
  usable for an interactively-called tool. So the index now **persists** to a
  per-workspace cache dir: built once, incrementally updated by
  `FileSignature{modified,size}`. README's "no persistent index" line becomes
  "semantic mode adds an opt-in persistent index; lexical/structural tools stay
  index-free."
- **Model download-on-first-use** into a cache dir (keeps the small-binary
  distribution intact); BYO model path as fallback.
- **Scope = the semantic/hybrid retrieval engine only.** Precise navigation
  (SCIP/LSP) and grep-ast structural context are deferred to separate plans.

## Background

Current state (from explore probes):

- **Pre-Phase-2 state was strictly in-memory, on-demand.** `FsCatalogProvider` holds an in-process
  `ProviderCache` (`crates/nerve-core/src/catalog.rs` ~`340`); snapshot is TTL-keyed,
  codemap cache is keyed by `FileSignature{modified,size}` (~`354`), invalidated at
  ~`422`. Phase 2 changes only semantic mode: lexical/structural tools remain
  on-demand, while `semantic_search` adds a per-workspace persistent index.
- **Fusion point already exists.** Content ranking is applied in one place —
  `apply_content_relevance_scores` (`crates/nerve-core/src/search.rs` ~`694`), with a
  BM25 / TF-density switch (~`705`). A dense+rerank stage fuses here (RRF) rather
  than rewriting search.
- **Tool surface pattern.** New tool = `tool_specs` entry + dispatch match arm +
  `ToolText` impl (`crates/nerve-core/src/dispatch.rs`; `file_search` at ~`447`,
  `FileSearchArgs` ~`1113`). `search_snapshot_cancellable` is the search entry
  (`search.rs:36`). We already extract tree-sitter symbols (`code_symbols_for_path`
  on the `CatalogProvider` trait, `port.rs:63`) — the natural chunk unit.
- **serve wiring.** `ServeArgs` (`crates/nerve-workstation/src/main.rs` ~`43-52`) has
  `--root/--workspace/--max-entries`; no cache-dir flag yet.

Open-source stack (from research, June 2026):

- **Inference:** `fastembed-rs` (Apache-2.0) — batteries-included local ONNX
  embeddings **and** `TextRerank`, downloads weights once into a cache then runs
  offline, accepts user-supplied ONNX. Backed by `ort` (ONNX Runtime, MIT) for the
  lower-level path. `candle` is safetensors-native, less direct for ONNX.
- **Embedding model:** `jina-embeddings-v2-base-code` (161M, 768-dim, 8192 tokens,
  **Apache-2.0**, <300MB ONNX) — best quality/size/license fit for code. Tiny
  fallback: `bge-small-en-v1.5` int8 (~32MB, not code-specific → lean on BM25).
- **Reranker:** `bge-reranker-base` (278M, **MIT**, quantize from ~1.1GB fp32),
  rerank only top 50–100.
- **ANN:** `hnsw_rs` (MIT/Apache, pure Rust, dump/reload + mmap) or `usearch`
  (Apache-2.0, native, save/load) — both persist to disk.
- **Chunking:** symbol/function chunks via tree-sitter (we already have them) >
  fixed windows; carry path + symbol + signature metadata.
- **Consensus stack:** BM25/lexical + dense → RRF fusion → cross-encoder rerank
  top-k. Rerank lift commonly ~10–20%; hybrid > either alone (Cursor, Qdrant,
  Weaviate, Pinecone, BGE).
- **Avoid (license):** SFR/CodeXEmbed (CC-BY-NC), jina-reranker-v2 (CC-BY-NC),
  Qodo-Embed (OpenRAIL restrictions) for commercial bundling.

## Approach

Add semantic retrieval as an **additive, feature-gated (`semantic`), non-wasm
subsystem** in `nerve-core`, exposed by a new `semantic_search` MCP tool. Existing
`file_search` is untouched; the default build stays small and dependency-free.

Pipeline (consensus hybrid stack): **codemap-symbol chunks → local ONNX embeddings
(`fastembed`) → ANN (`hnsw_rs`) candidates ⊕ chunk-level BM25 candidates → RRF
fusion → optional cross-encoder rerank (top 50–100) → snippet materialization.**
`mode: hybrid` is the default; `semantic` skips BM25.

v1 now persists the semantic index: the first `semantic_search` builds chunks and
embeddings, then subsequent sessions load the persisted artifacts and incrementally
re-embed only files whose `FileSignature{modified,size}` changed. A deterministic
**mock embedding/reranker backend** keeps tests/CI offline; the real model downloads
on first use into a separate model cache dir. Chunks remain sub-file, so the
persisted manifest carries the `file → chunk-id` map used for tombstones and
incremental re-embedding.

Supporting refactor: extract the **tokenizer + scoring primitives** from
`search.rs` into a shared `ranking` module (`file_search` byte-identical — goldens
prove it). This is *not* a free reuse: the existing ranker is deliberately
file-level and **IDF-free** (its own doc comment, `search.rs` ~`597`), so
chunk-level BM25 needs its own corpus model (IDF + avg-length over chunks) — treat
it as real work, not a wrapper.

Key attachment points (seams):
- `apply_content_relevance_scores` / BM25 in `search.rs` (~`694`) → shared `ranking`.
- `code_symbols_for_path` (`port.rs:63`) + `codemap::block_span` → the chunk unit.
- `FileSignature{modified,size}` (`catalog.rs` ~`354`) → the persisted
  `file → chunk-id` manifest freshness key.
- New feature-gated `semantic_index() -> Option<Arc<SemanticIndex>>` default method
  (returns `None`) on `CatalogProvider` (`port.rs`).
- Tool pattern in `dispatch.rs` (`file_search` arm ~`447`) → `semantic_search` arm + `ToolText`.
- `ServeArgs` (`crates/nerve-workstation/src/main.rs` ~`43`) → `--semantic-*` flags flowing into registry/providers.
- New request/response types in `models.rs`, following the existing transport-neutral pattern.

## Work Items

MVP = prove hybrid retrieval quality with an **in-memory** index (no persistence).
Types and the index core come before provider wiring (a field can't precede its type).

1. **Feature gate + deps skeleton.** `nerve-core` `semantic` feature (non-wasm):
   `fastembed`, `hnsw_rs`, `sha2`. `nerve-workstation` `semantic = ["nerve-core/semantic"]`.
   Prove the default (non-semantic) build/footprint is unchanged.
2. **Shared `ranking` module.** Extract tokenizer + scoring primitives from
   `search.rs` (`file_search` byte-identical, goldens prove it); expose
   binary-sniff / glob-filter as `pub(crate)`.
3. **Schemas.** `SemanticSearchRequest/Response/Result` + `NerveError::SemanticUnavailable`
   in `models.rs`. No dispatch yet.
4. **Chunker** (`semantic/chunk.rs`): codemap symbols + `block_span` spans, fixed
   line-window fallback, metadata (path/symbol/signature), content-hash chunk IDs,
   and the in-memory `file → chunk-id` map. Unit-tested.
5. **Index core**: embedding/reranker behind a trait with a **deterministic mock**
   (CI offline); `hnsw_rs` ANN; **chunk-BM25 corpus** (its own IDF/avg-length over
   chunks — not the file-level ranker).
6. **Query pipeline**: dense ANN ⊕ chunk-BM25 → RRF → optional rerank (top 50–100)
   → re-read snippets through the provider. `mode: hybrid` default, `semantic` skips BM25.
7. **MCP surface + CLI**: `semantic_index()` on the provider; `semantic_search`
   tool (spec + arm + `ToolText`); `--semantic-index` / `--semantic-embedding-model`
   / `--semantic-reranker-model` / `--semantic-no-rerank` into the registry; tests
   (listing, disabled-runtime, mock-backed dispatch).
8. **Wire real models + eval**: `jina-embeddings-v2-base-code` (Apache-2.0) +
   `bge-reranker-base` (MIT) via `fastembed`; README/parity docs; an eval harness
   over fixtures + a sample repo measuring recall and latency, rerank on/off.

## Phase 2 — Persistence + Incremental Index (now v1; builds on Items 1–8)

Driver: the measured ~4.6 min cold first query (above). Goal: pay the embedding
cost **once**, then load + incrementally update on subsequent sessions.

9. **Cache layout + manifest** under `--semantic-cache-dir` (default a
   per-workspace dir). Workspace key = SHA-256 over canonical roots + embedding
   model id + embedding dim + chunker version + schema version. Persist three
   artifacts: chunk metadata, embeddings (raw `f32`), ANN graph.
10. **Atomic writes**: temp file → fsync → rename, so a crash never leaves a
    half-written index.
11. **Incremental load/update**: on first query, load manifest; per file compare
    `FileSignature{modified,size}`; embed only new/changed files; tombstone chunks
    of removed/changed files; rebuild the ANN from active embeddings when needed.
12. **Compaction**: when tombstones exceed a threshold (e.g. 20% or 10k), rebuild
    embeddings/chunk arrays + ANN from active chunks and atomically swap.
13. **Corruption / version mismatch → rebuild**: `schema_version` + `chunker_version`
    + model-id mismatch trigger a clean rebuild rather than partial migration.
14. **Tests** (mock backend, no real model): save/load round-trip, stale-file
    re-embed, tombstone + compaction, corruption → rebuild, version-bump rebuild.

## Phase 3 — Cold-build mitigations (chosen)

Measured: cold build is embedding-bound (~85s for ~910 chunks of `nerve-core/src`,
release, CPU already saturated — intra-thread tuning was a verified no-op).
Persistence makes that a one-time cost; these two reduce it further and hide it.

15. **Index-scope filtering.** By default exclude tests, vendored/generated code,
    build artifacts, and docs from the *semantic index* (they're rarely retrieval
    targets and inflate chunk count). Reuse the `file_search` `extensions` /
    `include` / `exclude` glob machinery (already in `search.rs`); make the index
    scope configurable. Fewer chunks → proportional cold-build cut **and** cleaner
    recall. Cheapest, biggest win.
16. **Background build + BM25 fallback.** Build the dense index on a background
    thread (non-blocking); the first `semantic_search` returns **BM25-only**
    results immediately with a "dense index warming" diagnostic, auto-upgrading to
    full hybrid once the index is ready. Doesn't reduce compute — removes the
    user-facing wait, which (with persistence) is what actually matters. Needs a
    small index-state machine (building / ready) + concurrency care.

### Follow-up (still deferred)

- **`build_context` integration**: let the deterministic builder draw on semantic
  candidates once the tool is stable.
- **Further latency mitigations** (only if cold/large repos still hurt after
  Phase 3): int8-quantized `jina` (rerank absorbs the ~1–5% recall hit) or a
  `bge-small` fast tier; CoreML EP (Mac-only). Measure on our corpus, don't
  decide on generic numbers.

## Open Questions

- The four assumptions above are overridable; the load-bearing one (persistence)
  is resolved by Phase 2's persisted semantic cache.
- **Chunk-BM25 corpus cost**: building IDF/avg-length over all chunks happens on
  the same pass that embeds — confirm the combined first-query latency is
  acceptable on a large repo (first build still pays this cost; warm sessions should not).
- **Spike before Work Item 8**: does `fastembed` load `jina-embeddings-v2-base-code`
  directly or need a custom ONNX config, and is its model cache dir controllable
  without a global env var?
- ANN crate: `hnsw_rs` (pure Rust, no native link step) is the default over
  `usearch` (native, faster); revisit only if scale forces it.

## References

- Augment context engine architecture: <https://www.augmentcode.com/blog/a-real-time-index-for-your-codebase-secure-personal-scalable>
- Cursor semantic search (complements grep): <https://cursor.com/blog/semsearch>
- Claude Context (hybrid BM25+dense, AST chunking): <https://github.com/zilliztech/claude-context>
- `fastembed-rs`: <https://docs.rs/fastembed/latest/fastembed/> · `ort`: <https://docs.rs/ort/latest/ort/>
- `hnsw_rs`: <https://docs.rs/hnsw_rs/latest/hnsw_rs/> · `usearch`: <https://docs.rs/usearch/latest/usearch/>
- `jina-embeddings-v2-base-code`: <https://huggingface.co/jinaai/jina-embeddings-v2-base-code> · `bge-reranker-base`: <https://huggingface.co/BAAI/bge-reranker-base>
- Reranking best practice: <https://www.pinecone.io/learn/series/rag/rerankers/>
