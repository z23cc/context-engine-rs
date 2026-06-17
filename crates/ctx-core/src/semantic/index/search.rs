use super::*;

pub(super) fn search_built(
    index: &SemanticIndex,
    built: &BuiltSemanticIndex,
    generation: u64,
    scanned_files: usize,
    request: &SemanticSearchRequest,
    cancel: &CancelToken,
) -> Result<SemanticSearchResponse, CtxError> {
    let max_results = request.max_results.max(1);
    let candidate_limit = index.config.candidates.max(max_results);
    let query_vector = index.embedding.embed_query(&request.query)?;
    cancel.check_cancelled()?;

    let dense = built.ann.search(&query_vector, candidate_limit);
    let bm25 = if request.mode == SemanticSearchMode::Hybrid {
        built.bm25.search(&request.query, candidate_limit)
    } else {
        Vec::new()
    };
    let mut fused = rrf_fuse(&dense, &bm25);
    fused.sort_by(|left, right| {
        rank_cmp(left.1, right.1)
            .then_with(|| chunk_cmp(&built.chunks[left.0], &built.chunks[right.0]))
    });
    fused.truncate(candidate_limit);

    let mut scored: Vec<(usize, f64)> = fused;
    let mut reranked = 0usize;
    if request.rerank
        && index.config.rerank
        && let Some(reranker) = &index.reranker
    {
        // Only rerank a bounded window around the results we would return.
        // Reranking the full candidate pool and then truncating to
        // `max_results` lets a mediocre cross-encoder promote deep-pool junk
        // into the top-k and evict good fused hits (measured: recall drop).
        // Capping the window keeps rerank as an order-refiner, not a recall risk.
        let rerank_window = max_results.saturating_mul(RERANK_WINDOW_FACTOR);
        let rerank_limit = index
            .config
            .rerank_limit
            .min(scored.len())
            .min(rerank_window.max(max_results));
        let docs: Vec<String> = scored
            .iter()
            .take(rerank_limit)
            .map(|(idx, _)| built.chunks[*idx].text.clone())
            .collect();
        let rerank_scores = reranker.rerank(&request.query, &docs)?;
        reranked = rerank_scores.len().min(rerank_limit);
        for ((_, score), rerank_score) in scored.iter_mut().take(reranked).zip(rerank_scores) {
            *score = rerank_score as f64;
        }
        scored[..reranked].sort_by(|left, right| {
            rank_cmp(left.1, right.1)
                .then_with(|| chunk_cmp(&built.chunks[left.0], &built.chunks[right.0]))
        });
    }

    scored.truncate(max_results);
    let results = scored
        .iter()
        .map(|(idx, score)| chunk_to_result(&built.chunks[*idx], *score))
        .collect();
    Ok(SemanticSearchResponse {
        generation,
        index_state: SemanticIndexState::Ready,
        results,
        diagnostics: built.diagnostics.clone(),
        totals: SemanticSearchTotals {
            scanned_files,
            chunks: built.chunks.len(),
            dense_candidates: dense.len(),
            bm25_candidates: bm25.len(),
            fused_candidates: scored.len(),
            reranked,
        },
    })
}

pub(super) fn search_fallback(
    index: &SemanticIndex,
    chunk_build: ChunkBuild,
    scanned_files: usize,
    request: &SemanticSearchRequest,
) -> Result<SemanticSearchResponse, CtxError> {
    let max_results = request.max_results.max(1);
    let candidate_limit = index.config.candidates.max(max_results);
    let bm25 = if request.mode == SemanticSearchMode::Hybrid {
        let bm25_index = ChunkBm25::new(&chunk_build.chunks);
        bm25_index.search(&request.query, candidate_limit)
    } else {
        Vec::new()
    };
    let index_state = if request.mode == SemanticSearchMode::Hybrid {
        SemanticIndexState::Bm25Only
    } else {
        SemanticIndexState::Warming
    };
    let mut scored = bm25.clone();
    scored.truncate(max_results);
    let results = scored
        .iter()
        .map(|(idx, score)| chunk_to_result(&chunk_build.chunks[*idx], *score))
        .collect();
    let mut diagnostics = chunk_build.diagnostics;
    diagnostics.push(Diagnostic {
        path: None,
        message: if request.mode == SemanticSearchMode::Hybrid {
            "dense semantic index warming; returning BM25-only results".to_string()
        } else {
            "dense semantic index warming; semantic-only mode has no fallback results".to_string()
        },
    });
    if let Some(error) = index
        .last_build_error
        .lock()
        .expect("semantic build error lock")
        .clone()
    {
        diagnostics.push(Diagnostic {
            path: None,
            message: format!("dense semantic index build failed: {error}"),
        });
    }
    Ok(SemanticSearchResponse {
        generation: chunk_build.generation,
        index_state,
        results,
        diagnostics,
        totals: SemanticSearchTotals {
            scanned_files,
            chunks: chunk_build.chunks.len(),
            dense_candidates: 0,
            bm25_candidates: bm25.len(),
            fused_candidates: scored.len(),
            reranked: 0,
        },
    })
}
