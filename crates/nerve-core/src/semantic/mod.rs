//! Feature-gated semantic + hybrid retrieval.

pub(crate) mod chunk;

use self::chunk::{CHUNKER_VERSION, ChunkBuild, SemanticChunk, build_chunks_for_entries};
use crate::{
    cancel::CancelToken,
    models::{
        CatalogEntry, Diagnostic, NerveError, RootRef, SemanticIndexState, SemanticSearchMode,
        SemanticSearchRequest, SemanticSearchResponse, SemanticSearchResult, SemanticSearchTotals,
    },
    port::{CatalogProvider, FileSignature},
    ranking::{EntryFilter, EntryFilterConfig, tokenize_query, tokenize_text},
    snapshot::CatalogSnapshot,
};
use fastembed::{
    EmbeddingModel, RerankInitOptions, RerankerModel, TextEmbedding, TextInitOptions, TextRerank,
};
use hnsw_rs::prelude::{AnnT, DistCosine, Hnsw};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

mod ann;
mod backend;
mod bm25;
mod config;
mod index;
mod persistence;
mod persistence_rebuild;
mod scoring;

pub use backend::{
    FastembedEmbeddingBackend, FastembedRerankerBackend, MockEmbeddingBackend, MockRerankerBackend,
};
pub use config::{
    EmbeddingBackend, GenerationClock, RerankerBackend, SemanticIndexConfig, SemanticIndexScope,
    SemanticPersistenceConfig, SemanticRuntimeConfig,
};
pub use index::{SemanticIndex, SemanticWarmResponse};

use ann::*;
use bm25::*;
use config::*;
use index::*;
use persistence::*;
use persistence_rebuild::*;
use scoring::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, HostFile, MemoryCatalogProvider, RootPolicy, ScanOptions};
    use std::{
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        thread,
        time::Duration,
    };

    #[test]
    fn chunk_bm25_uses_idf_over_chunks() {
        let chunks = vec![
            SemanticChunk {
                id: "a".into(),
                root_id: "r".into(),
                path: "a.rs".into(),
                display_path: "a.rs".into(),
                line_start: 1,
                line_end: 1,
                symbol: None,
                signature: None,
                text: "rare common".into(),
            },
            SemanticChunk {
                id: "b".into(),
                root_id: "r".into(),
                path: "b.rs".into(),
                display_path: "b.rs".into(),
                line_start: 1,
                line_end: 1,
                symbol: None,
                signature: None,
                text: "common common common".into(),
            },
        ];
        let bm25 = ChunkBm25::new(&chunks);
        let results = bm25.search("rare common", 2);
        assert_eq!(results[0].0, 0);
    }

    #[test]
    fn mock_semantic_index_finds_intent_text() {
        let provider = MemoryCatalogProvider::new(vec![
            HostFile::new(
                "alpha.rs",
                b"pub fn parse_config() { validate_config(); }".to_vec(),
            ),
            HostFile::new(
                "beta.rs",
                b"pub fn render_view() { draw_button(); }".to_vec(),
            ),
        ])
        .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        let index = SemanticIndex::mock();
        let response = index
            .search(
                &provider,
                &snapshot,
                &SemanticSearchRequest {
                    query: "config validation".into(),
                    ..SemanticSearchRequest::default()
                },
                &CancelToken::never(),
            )
            .expect("semantic search");
        assert!(!response.results.is_empty());
        assert_eq!(response.results[0].path, "alpha.rs");
        assert!(response.totals.chunks >= 2);
    }

    #[test]
    fn default_scope_excludes_noise_from_semantic_index() {
        let provider = MemoryCatalogProvider::new(vec![
            HostFile::new("src/lib.rs", b"pub fn live_code() { needle(); }".to_vec()),
            HostFile::new(
                "tests/test.rs",
                b"pub fn test_code() { needle(); }".to_vec(),
            ),
            HostFile::new("docs/guide.md", b"needle docs".to_vec()),
            HostFile::new("vendor/lib.rs", b"pub fn vendored() { needle(); }".to_vec()),
            HostFile::new("target/out.rs", b"pub fn built() { needle(); }".to_vec()),
            HostFile::new("build/out.rs", b"pub fn build_out() { needle(); }".to_vec()),
            HostFile::new("dist/bundle.rs", b"pub fn bundled() { needle(); }".to_vec()),
            HostFile::new("README.md", b"needle readme".to_vec()),
            HostFile::new(
                "src/foo_test.rs",
                b"pub fn test_file() { needle(); }".to_vec(),
            ),
            HostFile::new("src/foo.spec.ts", b"export const spec = 'needle';".to_vec()),
            HostFile::new(
                "src/generated/out.rs",
                b"pub fn generated() { needle(); }".to_vec(),
            ),
            HostFile::new(
                "src/schema.generated.rs",
                b"pub fn generated_file() { needle(); }".to_vec(),
            ),
            HostFile::new(
                "src/schema_generated.rs",
                b"pub fn generated_file_alt() { needle(); }".to_vec(),
            ),
        ])
        .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        let backend = Arc::new(CountingEmbeddingBackend::default());
        let index = SemanticIndex::new(
            SemanticIndexConfig {
                rerank: false,
                ..SemanticIndexConfig::default()
            },
            backend.clone(),
            None,
        );
        let response = index
            .search(
                &provider,
                &snapshot,
                &SemanticSearchRequest {
                    query: "needle".into(),
                    rerank: false,
                    ..SemanticSearchRequest::default()
                },
                &CancelToken::never(),
            )
            .expect("search");
        assert_eq!(backend.document_count(), 1);
        assert_eq!(response.totals.chunks, 1);
        assert!(
            response
                .results
                .iter()
                .all(|result| result.path == "src/lib.rs")
        );
    }

    #[test]
    fn scope_can_disable_default_excludes() {
        let provider = MemoryCatalogProvider::new(vec![
            HostFile::new("docs/guide.md", b"docs only needle".to_vec()),
            HostFile::new("src/lib.rs", b"pub fn live_code() {}".to_vec()),
        ])
        .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        let backend = Arc::new(CountingEmbeddingBackend::default());
        let index = SemanticIndex::new(
            SemanticIndexConfig {
                rerank: false,
                scope: SemanticIndexScope {
                    use_default_excludes: false,
                    include: vec!["docs/**".to_string()],
                    ..SemanticIndexScope::default()
                },
                ..SemanticIndexConfig::default()
            },
            backend.clone(),
            None,
        );
        let response = index
            .search(
                &provider,
                &snapshot,
                &SemanticSearchRequest {
                    query: "needle".into(),
                    rerank: false,
                    ..SemanticSearchRequest::default()
                },
                &CancelToken::never(),
            )
            .expect("search");
        assert_eq!(backend.document_count(), 1);
        assert_eq!(response.results[0].path, "docs/guide.md");
    }

    #[test]
    fn background_search_returns_bm25_fallback_then_dense_results() {
        let provider = MemoryCatalogProvider::new(vec![
            HostFile::new(
                "src/config.rs",
                b"pub fn parse_config() { validate_config(); }".to_vec(),
            ),
            HostFile::new(
                "src/view.rs",
                b"pub fn render_view() { draw_button(); }".to_vec(),
            ),
        ])
        .expect("provider");
        let snapshot = Arc::new(provider.snapshot().expect("snapshot"));
        let backend = Arc::new(SlowEmbeddingBackend::default());
        let index = Arc::new(SemanticIndex::new(
            SemanticIndexConfig {
                rerank: false,
                ..SemanticIndexConfig::default()
            },
            backend,
            None,
        ));
        let request = SemanticSearchRequest {
            query: "config validation".into(),
            rerank: false,
            ..SemanticSearchRequest::default()
        };

        let first = index
            .search_background(
                provider.clone(),
                Arc::clone(&snapshot),
                &request,
                &CancelToken::never(),
            )
            .expect("fallback search");
        assert_eq!(first.generation, snapshot.generation);
        assert_eq!(first.index_state, SemanticIndexState::Bm25Only);
        assert_eq!(first.totals.dense_candidates, 0);
        assert!(first.totals.bm25_candidates > 0);
        assert!(
            first
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("dense semantic index warming"))
        );

        let semantic_only = index
            .search_background(
                provider.clone(),
                Arc::clone(&snapshot),
                &SemanticSearchRequest {
                    mode: SemanticSearchMode::Semantic,
                    ..request.clone()
                },
                &CancelToken::never(),
            )
            .expect("semantic-only fallback search");
        assert_eq!(semantic_only.generation, snapshot.generation);
        assert_eq!(semantic_only.index_state, SemanticIndexState::Warming);
        assert!(semantic_only.results.is_empty());
        assert_eq!(semantic_only.totals.bm25_candidates, 0);
        assert!(semantic_only.diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("semantic-only mode has no fallback results")
        }));

        let mut ready = None;
        for _ in 0..20 {
            thread::sleep(Duration::from_millis(25));
            let response = index
                .search_background(
                    provider.clone(),
                    Arc::clone(&snapshot),
                    &request,
                    &CancelToken::never(),
                )
                .expect("ready search");
            if response.totals.dense_candidates > 0 {
                ready = Some(response);
                break;
            }
        }
        let ready = ready.expect("dense index becomes ready");
        assert_eq!(ready.generation, snapshot.generation);
        assert_eq!(ready.index_state, SemanticIndexState::Ready);
        assert_eq!(ready.results[0].path, "src/config.rs");
        assert!(
            ready
                .diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("dense semantic index warming"))
        );
    }

    #[test]
    fn persistent_cache_save_load_round_trip_with_mock_backend() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_file(workspace.path(), "alpha.txt", "parse config validate\n");
        let cache = tempfile::tempdir().expect("cache");
        let (provider, config) = fs_provider_with_cache(workspace.path(), cache.path());
        let first = SemanticIndex::new(
            semantic_config(config.clone()),
            Arc::new(MockEmbeddingBackend::default()),
            None,
        );
        let first_response = search(&first, &provider, "config validate");
        assert_eq!(first_response.results[0].path, "alpha.txt");
        assert!(current_path(&config).exists());
        let manifest = manifest(&config);
        let generation_dir = manifest_generation_dir(&config);
        assert!(
            generation_dir
                .join(format!("{}.hnsw.graph", manifest.ann.path))
                .exists()
        );
        assert!(
            generation_dir
                .join(format!("{}.hnsw.data", manifest.ann.path))
                .exists()
        );

        let second = SemanticIndex::new(
            semantic_config(config),
            Arc::new(MockEmbeddingBackend::default()),
            None,
        );
        let second_response = search(&second, &provider, "config validate");
        assert_eq!(second_response.results[0].path, "alpha.txt");
    }

    #[test]
    fn persistent_cache_load_avoids_unchanged_document_embedding() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_file(workspace.path(), "alpha.txt", "parse config validate\n");
        let cache = tempfile::tempdir().expect("cache");
        let (provider, config) = fs_provider_with_cache(workspace.path(), cache.path());
        let first_backend = Arc::new(CountingEmbeddingBackend::default());
        let first = SemanticIndex::new(semantic_config(config.clone()), first_backend, None);
        search(&first, &provider, "config validate");

        let second_backend = Arc::new(CountingEmbeddingBackend::default());
        let second = SemanticIndex::new(semantic_config(config), second_backend.clone(), None);
        search(&second, &provider, "config validate");
        assert_eq!(second_backend.document_count(), 0);
    }

    #[test]
    fn stale_file_reembeds_only_changed_file() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_file(workspace.path(), "alpha.txt", "parse config validate\n");
        write_file(workspace.path(), "beta.txt", "render button view\n");
        let cache = tempfile::tempdir().expect("cache");
        let (provider, config) = fs_provider_with_cache(workspace.path(), cache.path());
        let first = SemanticIndex::new(
            semantic_config(config.clone()),
            Arc::new(CountingEmbeddingBackend::default()),
            None,
        );
        search(&first, &provider, "config validate");

        write_file(workspace.path(), "beta.txt", "render button view updated\n");
        provider.invalidate();
        let backend = Arc::new(CountingEmbeddingBackend::default());
        let second = SemanticIndex::new(semantic_config(config), backend.clone(), None);
        search(&second, &provider, "updated");
        assert_eq!(backend.document_count(), 1);
    }

    #[test]
    fn removed_file_tombstones_and_compaction_removes_them() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_file(workspace.path(), "old.txt", "obsolete unique needle\n");
        write_file(workspace.path(), "live.txt", "active live code\n");
        write_file(workspace.path(), "extra.txt", "extra live code\n");
        let cache = tempfile::tempdir().expect("cache");
        let (provider, config) = fs_provider_with_cache(workspace.path(), cache.path());
        let first = SemanticIndex::new(
            semantic_config(config.clone()),
            Arc::new(MockEmbeddingBackend::default()),
            None,
        );
        search(&first, &provider, "obsolete");

        fs::remove_file(workspace.path().join("old.txt")).expect("remove old");
        provider.invalidate();
        let mut no_compact = semantic_config(config.clone());
        no_compact.compaction_tombstone_ratio = 1.0;
        no_compact.compaction_tombstone_count = usize::MAX;
        let second =
            SemanticIndex::new(no_compact, Arc::new(MockEmbeddingBackend::default()), None);
        let response = search(&second, &provider, "obsolete");
        assert!(
            response
                .results
                .iter()
                .all(|result| result.path != "old.txt")
        );
        assert_eq!(manifest(&config).tombstone_count, 1);

        fs::remove_file(workspace.path().join("extra.txt")).expect("remove extra");
        provider.invalidate();
        let mut compact = semantic_config(config.clone());
        compact.compaction_tombstone_ratio = 0.1;
        compact.compaction_tombstone_count = 1;
        let third = SemanticIndex::new(compact, Arc::new(MockEmbeddingBackend::default()), None);
        search(&third, &provider, "live");
        assert_eq!(manifest(&config).tombstone_count, 0);
    }

    #[test]
    fn corrupt_manifest_rebuilds_cleanly() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_file(workspace.path(), "alpha.txt", "parse config validate\n");
        let cache = tempfile::tempdir().expect("cache");
        let (provider, config) = fs_provider_with_cache(workspace.path(), cache.path());
        let first = SemanticIndex::new(
            semantic_config(config.clone()),
            Arc::new(MockEmbeddingBackend::default()),
            None,
        );
        search(&first, &provider, "config");
        let manifest_path = manifest_path(&config);
        fs::write(&manifest_path, b"not json").expect("corrupt manifest");

        let backend = Arc::new(CountingEmbeddingBackend::default());
        let second = SemanticIndex::new(semantic_config(config), backend.clone(), None);
        search(&second, &provider, "config");
        assert_eq!(backend.document_count(), 1);
    }

    #[test]
    fn version_mismatch_rebuilds_cleanly() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_file(workspace.path(), "alpha.txt", "parse config validate\n");
        let cache = tempfile::tempdir().expect("cache");
        let (provider, config) = fs_provider_with_cache(workspace.path(), cache.path());
        let first = SemanticIndex::new(
            semantic_config(config.clone()),
            Arc::new(MockEmbeddingBackend::default()),
            None,
        );
        search(&first, &provider, "config");
        let path = manifest_path(&config);
        let mut manifest = manifest(&config);
        manifest.schema_version = SCHEMA_VERSION + 1;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&manifest).expect("manifest"),
        )
        .expect("write manifest");

        let backend = Arc::new(CountingEmbeddingBackend::default());
        let second = SemanticIndex::new(semantic_config(config), backend.clone(), None);
        search(&second, &provider, "config");
        assert_eq!(backend.document_count(), 1);
    }

    #[derive(Default)]
    struct SlowEmbeddingBackend {
        inner: MockEmbeddingBackend,
    }

    impl EmbeddingBackend for SlowEmbeddingBackend {
        fn dimension(&self) -> usize {
            self.inner.dimension()
        }

        fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, NerveError> {
            thread::sleep(Duration::from_millis(100));
            self.inner.embed_documents(texts)
        }

        fn embed_query(&self, query: &str) -> Result<Vec<f32>, NerveError> {
            self.inner.embed_query(query)
        }
    }

    #[derive(Default)]
    struct CountingEmbeddingBackend {
        inner: MockEmbeddingBackend,
        document_count: AtomicUsize,
    }

    impl CountingEmbeddingBackend {
        fn document_count(&self) -> usize {
            self.document_count.load(AtomicOrdering::SeqCst)
        }
    }

    impl EmbeddingBackend for CountingEmbeddingBackend {
        fn dimension(&self) -> usize {
            self.inner.dimension()
        }

        fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, NerveError> {
            self.document_count
                .fetch_add(texts.len(), AtomicOrdering::SeqCst);
            self.inner.embed_documents(texts)
        }

        fn embed_query(&self, query: &str) -> Result<Vec<f32>, NerveError> {
            self.inner.embed_query(query)
        }
    }

    fn write_file(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
    }

    fn fs_provider_with_cache(
        root: &Path,
        cache: &Path,
    ) -> (FsCatalogProvider, SemanticPersistenceConfig) {
        let policy = RootPolicy::new(vec![root.to_path_buf()]).expect("policy");
        let roots = policy.roots().to_vec();
        let config = semantic_persistence_config(
            Some(cache),
            &roots,
            "mock",
            MockEmbeddingBackend::default().dimension(),
            &SemanticIndexScope::default(),
        )
        .expect("persistence")
        .expect("enabled");
        (
            FsCatalogProvider::new(policy, ScanOptions::default()),
            config,
        )
    }

    fn semantic_config(persistence: SemanticPersistenceConfig) -> SemanticIndexConfig {
        SemanticIndexConfig {
            persistence: Some(persistence),
            rerank: false,
            ..SemanticIndexConfig::default()
        }
    }

    fn search(
        index: &SemanticIndex,
        provider: &FsCatalogProvider,
        query: &str,
    ) -> SemanticSearchResponse {
        let snapshot = provider.snapshot().expect("snapshot");
        index
            .search(
                provider,
                &snapshot,
                &SemanticSearchRequest {
                    query: query.to_string(),
                    rerank: false,
                    ..SemanticSearchRequest::default()
                },
                &CancelToken::never(),
            )
            .expect("search")
    }

    fn manifest_path(config: &SemanticPersistenceConfig) -> PathBuf {
        let current: PersistedCurrent = read_json(&current_path(config)).expect("current");
        generation_dir(config, &current.generation).join("manifest.json")
    }

    fn manifest(config: &SemanticPersistenceConfig) -> PersistedManifest {
        read_json(&manifest_path(config)).expect("manifest")
    }

    fn manifest_generation_dir(config: &SemanticPersistenceConfig) -> PathBuf {
        let current: PersistedCurrent = read_json(&current_path(config)).expect("current");
        generation_dir(config, &current.generation)
    }
}
