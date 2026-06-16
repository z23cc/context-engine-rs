#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use ctx_core::{
        CancelToken, CatalogProvider, FsCatalogProvider, RootPolicy, ScanOptions,
        SemanticSearchMode, SemanticSearchRequest, semantic::SemanticRuntimeConfig,
    };
    use std::{path::PathBuf, time::Instant};

    let mut args = std::env::args().skip(1);
    let root = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("crates/ctx-core/tests/fixtures"));
    let query = args
        .next()
        .unwrap_or_else(|| "configuration validation".to_string());
    let use_real_models = args.any(|arg| arg == "--real");

    let semantic = if use_real_models {
        SemanticRuntimeConfig {
            enabled: true,
            embedding_model: None,
            reranker_model: None,
            model_cache_dir: None,
            index_cache_dir: None,
            rerank: true,
            mock: false,
            scope: Default::default(),
        }
    } else {
        SemanticRuntimeConfig::mock()
    };
    let policy = RootPolicy::new(vec![root])?;
    let index = semantic
        .build_index_for_roots(policy.roots())?
        .expect("semantic eval enables semantic index");
    let provider =
        FsCatalogProvider::with_semantic_index(policy, ScanOptions::default(), Some(index));
    let snapshot = provider.snapshot()?;

    for rerank in [false, true] {
        let started = Instant::now();
        let response = provider.semantic_index().expect("semantic index").search(
            &provider,
            &snapshot,
            &SemanticSearchRequest {
                query: query.clone(),
                mode: SemanticSearchMode::Hybrid,
                max_results: 10,
                rerank,
            },
            &CancelToken::never(),
        )?;
        println!(
            "rerank={rerank} elapsed_ms={} chunks={} results={}",
            started.elapsed().as_millis(),
            response.totals.chunks,
            response.results.len()
        );
        for result in response.results.iter().take(5) {
            println!(
                "  {:.4}\t{}:{}-{}\t{}",
                result.score,
                result.display_path,
                result.line_start,
                result.line_end,
                result.symbol.as_deref().unwrap_or("")
            );
        }
    }

    Ok(())
}

#[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
fn main() {
    eprintln!(
        "semantic_eval requires: cargo run -p ctx-core --example semantic_eval --features semantic -- <root> <query> [--real]"
    );
    std::process::exit(2);
}
