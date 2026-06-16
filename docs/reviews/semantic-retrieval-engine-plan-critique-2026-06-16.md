# Critique: Semantic + Hybrid Retrieval Engine Plan

**Scope:** Reviews `docs/plans/semantic-retrieval-engine-2026-06-16.md`. Seam line
numbers spot-checked against `crates/ctx-core/src/{search,port,catalog}.rs` and are
accurate. Critique only — no scope additions.

## 1. Top 3 under-specified seams

1. **Chunk-level BM25 reuse (`search.rs` ~627–711).** The plan (Items 2, 9) treats
   "extract the BM25 helpers and reuse at chunk granularity" as a mechanical move.
   It isn't. The existing ranker is *deliberately* file-level and **IDF-free** — the
   `ContentRankingQuery` doc comment (`search.rs:627`) states "no cross-document IDF
   is needed to order files," and scoring runs only over the already-matched set
   (`bm25_file_scores`, `FileRankingStats` keyed by path string). Chunk retrieval
   wants chunks as documents *with* IDF + avgdl over the whole chunk corpus. The seam
   that must be specified is **the corpus model** (what is a document, where corpus
   stats live, how candidates are generated before scoring) — not "share the helper."

2. **`semantic_index()` on `CatalogProvider` (`port.rs`, Item 4).** Return type and
   target-gating are unspecified. What does it return — an `Arc<SemanticIndex>`, a
   handle, an `Option`? `CatalogProvider` is cross-target (wasm + FFI impls exist);
   the subsystem is non-wasm. A defaulted method avoids forcing other impls to change,
   but **feature-gating a trait method changes the trait's API surface per feature
   build** — that needs a one-line decision, not silence.

3. **Persistence / incremental keying (`FileSignature{modified,size}`, `catalog.rs:363`,
   Item 6).** `FileSignature` is **per-file**; chunks are sub-file. A file-level
   signature tells you *a file changed*, not *which chunk IDs to tombstone*. The real
   artifact — a **file→chunk-ID manifest** reconciling content-hash chunk IDs (Item 5)
   with the file signature (Item 6) — is the hard part and is hand-waved as "manifest."
   Items 5 and 6 use two different identity schemes that must be reconciled explicitly.

## 2. Contradictions / dependency gaps in the Work Items

- **Item 9 has an unowned dependency.** It calls for "chunk BM25," but Item 2 only
  guarantees *file_search byte-identical parity*. No item owns building a chunk-capable
  BM25 (the real work from seam #1). Either widen Item 2's charter or add an explicit
  chunk-ranker item — as written it falls through the cracks.
- **Item 4 is mis-ordered against 6/7/8.** It wires "an optional index field on
  `FsCatalogProvider`" and `semantic_index()`, but the index *type* isn't designed
  until persistence (6) + embeddings (7) + ANN (8). Item 4 must either forward-declare
  an opaque handle (say so) or move after 6–8.
- **Mock-backend ordering is fine.** Item 7 precedes its consumers (9 pipeline tests,
  10 mock-backed dispatch). No problem here.
- **Cache-dir plumbing is split:** persistence (6) needs a cache dir, but the
  `--semantic-cache-dir` flag arrives at Item 11. OK only if 6/7/8 test via temp dirs;
  state that assumption.

## 3. Over-planning — cut/merge for a first executable cut

13 items is too many to prove the stated priority (*retrieval quality / effect*).
The persistence half is the bulk of the risk and is **not** needed to demonstrate the
effect — and cutting it keeps the README's "on-demand, no persistent index"
positioning the plan otherwise frets about reversing.

**True MVP (~5 items):**
- Scaffold = merge **1 + 3 + 4** (feature gate, schemas, opaque provider field).
- Chunker = **5**.
- Embedding trait + mock + one real model = merge **7 + 12**.
- In-memory query pipeline: dense candidates ⊕ existing file-level BM25 → RRF, **no
  rerank, no persistence, brute-force or in-memory ANN** = subset of **9**.
- Tool arm = **10**.

**Defer to phase 2:** persistence (6), on-disk ANN (8), chunk-BM25 extraction (2),
cross-encoder rerank (in 9), CLI surface (11), full docs/parity (13, keep only a thin
recall/latency eval since effect is the priority).

## 4. Questions that change implementation order

1. **Is persistence in v1?** If no → cut 6, 8, most of 11; seam #3 (FileSignature/
   manifest/tombstone) leaves the critical path entirely. *This is the decisive question.*
2. **Does chunk BM25 need true corpus IDF/avgdl?** If yes, Item 2 is a misframe —
   you're building a new ranker, not extracting one; sequence it after the chunker as
   its own item rather than bundling into the parity-only refactor.
3. **Does `semantic_index()` compile cleanly on wasm/FFI impls?** Determines whether
   Item 4 is a one-line default or touches every `CatalogProvider` implementor.
4. **Brute-force cosine vs `hnsw_rs` for v1?** If the corpus is small, ANN (8) is
   premature; cutting it also shrinks persistence (6).
