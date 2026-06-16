# semantic_search parity notes

`semantic_search` is available only in native builds compiled with `--features semantic`.
Default builds do not list the tool and keep the existing on-demand/no-persistent-index behavior.

## Semantic-feature behavior

- Index lifetime: built on first `semantic_search`, then cached in memory for the provider/session.
- Persistence: semantic mode writes a per-workspace index cache containing chunk metadata, raw `f32` embeddings, and ANN metadata. Cache identity includes canonical roots, embedding model id, embedding dimension, chunker version, and schema version.
- Incrementality: subsequent sessions load the persisted cache and re-embed only files whose `FileSignature { modified, size }` changed; removed/changed file chunks are tombstoned and compacted past threshold.
- Cache dirs: `--semantic-cache-dir` controls workspace semantic index artifacts. `--semantic-model-cache-dir` controls fastembed model downloads only.
- Retrieval: code-symbol chunks with fixed-window fallback, dense ANN candidates, real chunk-level BM25 with IDF/avg chunk length, RRF fusion, optional rerank.
- Test backend: deterministic mock embeddings/reranker; CI must not download models.

## Model defaults

- Embedding: `jina-embeddings-v2-base-code` (`EmbeddingModel::JinaEmbeddingsV2BaseCode`).
- Reranker: `bge-reranker-base` (`RerankerModel::BGERerankerBase`).
- Supported CLI aliases also include full Hugging Face repo names.

## Eval harness

Mock/offline fixture run:

```bash
cargo run -p ctx-core --example semantic_eval --features semantic -- \
  crates/ctx-core/tests/fixtures "config validation"
```

Real-model sample repo run (downloads on first semantic use):

```bash
cargo run -p ctx-core --example semantic_eval --features semantic -- \
  /abs/sample/repo "where is auth handled" --real
```

The harness prints rerank on/off latency, chunk count, result count, and the top result spans.
