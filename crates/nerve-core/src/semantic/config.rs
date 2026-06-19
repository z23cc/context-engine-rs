use super::backend::{
    embedding_dimension, parse_embedding_model, parse_reranker_model, semantic_model_cache_dir,
};
use super::*;

pub(super) const DEFAULT_CANDIDATES: usize = 100;
pub(super) const DEFAULT_RERANK_LIMIT: usize = 100;
/// Rerank only the top `max_results * RERANK_WINDOW_FACTOR` fused candidates,
/// so the cross-encoder refines ordering near the cut without promoting
/// deep-pool junk into the returned top-k (which would drop recall@k).
pub(super) const RERANK_WINDOW_FACTOR: usize = 4;
pub(super) const RRF_K: f64 = 60.0;
pub(super) const HNSW_MAX_CONN: usize = 16;
pub(super) const HNSW_MAX_LAYER: usize = 16;
pub(super) const HNSW_EF_CONSTRUCTION: usize = 200;
pub(super) const HNSW_EF_SEARCH: usize = 128;
pub(super) const SCHEMA_VERSION: u32 = 1;
pub(super) const TOMBSTONE_RATIO_THRESHOLD: f64 = 0.20;
pub(super) const TOMBSTONE_COUNT_THRESHOLD: usize = 10_000;

pub trait EmbeddingBackend: Send + Sync {
    fn dimension(&self) -> usize;
    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, NerveError>;
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, NerveError>;
}

pub trait RerankerBackend: Send + Sync {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, NerveError>;
}

pub(super) const DEFAULT_SCOPE_EXCLUDES: &[&str] = &[
    "**/.git/**",
    "**/.build/**",
    "**/target/**",
    "**/build/**",
    "**/dist/**",
    "**/out/**",
    "**/coverage/**",
    "**/node_modules/**",
    "**/vendor/**",
    "**/Vendor/**",
    "**/third_party/**",
    "**/ThirdParty/**",
    "**/ThirdPartyLicenses/**",
    "**/tests/**",
    "**/Tests/**",
    "**/test/**",
    "**/__tests__/**",
    "**/*_test.*",
    "**/*.test.*",
    "**/*.spec.*",
    "**/docs/**",
    "**/Docs/**",
    "**/README*",
    "**/*.md",
    "**/*.mdx",
    "**/*.rst",
    "**/*.adoc",
    "**/generated/**",
    "**/Generated/**",
    "**/*_generated.*",
    "**/*.generated.*",
    "**/*Generated*",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticIndexScope {
    pub extensions: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub use_default_excludes: bool,
}

impl Default for SemanticIndexScope {
    fn default() -> Self {
        Self {
            extensions: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            use_default_excludes: true,
        }
    }
}

impl SemanticIndexScope {
    pub(super) fn filter_config(&self) -> EntryFilterConfig {
        let mut exclude = Vec::new();
        if self.use_default_excludes {
            exclude.extend(
                DEFAULT_SCOPE_EXCLUDES
                    .iter()
                    .map(|pattern| (*pattern).to_string()),
            );
        }
        exclude.extend(self.exclude.clone());
        EntryFilterConfig {
            extensions: self.extensions.clone(),
            include: self.include.clone(),
            exclude,
        }
    }

    pub(super) fn entry_filter(&self) -> Result<EntryFilter, NerveError> {
        EntryFilter::from_config(&self.filter_config())
    }

    pub(super) fn cache_fingerprint(&self) -> String {
        let config = self.filter_config();
        let mut hasher = Sha256::new();
        for values in [&config.extensions, &config.include, &config.exclude] {
            for value in values {
                hasher.update(value.as_bytes());
                hasher.update([0]);
            }
            hasher.update([0xff]);
        }
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Clone)]
pub struct SemanticIndexConfig {
    pub candidates: usize,
    pub rerank_limit: usize,
    pub rerank: bool,
    pub persistence: Option<SemanticPersistenceConfig>,
    pub scope: SemanticIndexScope,
    pub compaction_tombstone_ratio: f64,
    pub compaction_tombstone_count: usize,
}

#[derive(Clone, Debug)]
pub struct SemanticPersistenceConfig {
    pub cache_base_dir: PathBuf,
    pub workspace_key: String,
    pub roots: Vec<PathBuf>,
    pub embedding_model_id: String,
    pub embedding_dimension: usize,
    /// Source of the unique suffix used in persisted generation ids and temp-file
    /// names. Injectable so the kernel never hard-codes a wall-clock read; defaults
    /// to process id + wall-clock nanoseconds.
    pub generation_clock: GenerationClock,
}

/// Injectable source of the unique suffix used in persisted generation ids and
/// temp-file names. The default reads the process id and wall-clock nanoseconds;
/// hosts or tests can inject a deterministic source so nerve-core's persist path
/// never hard-codes a wall-clock read. The produced string is opaque — only its
/// uniqueness matters — so the on-disk persisted format stays compatible.
#[derive(Clone)]
pub struct GenerationClock(Arc<dyn Fn() -> String + Send + Sync>);

impl GenerationClock {
    /// Build a clock from a closure yielding a unique generation id on each call.
    #[must_use]
    pub fn new(source: impl Fn() -> String + Send + Sync + 'static) -> Self {
        Self(Arc::new(source))
    }

    /// Produce the next opaque, unique generation id.
    pub(crate) fn generation_id(&self) -> String {
        (self.0)()
    }
}

impl Default for GenerationClock {
    fn default() -> Self {
        Self(Arc::new(|| {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            format!("{}-{nanos}", std::process::id())
        }))
    }
}

impl std::fmt::Debug for GenerationClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("GenerationClock").finish()
    }
}

#[derive(Clone, Debug)]
pub struct SemanticRuntimeConfig {
    pub enabled: bool,
    pub embedding_model: Option<String>,
    pub reranker_model: Option<String>,
    pub model_cache_dir: Option<PathBuf>,
    pub index_cache_dir: Option<PathBuf>,
    pub rerank: bool,
    pub mock: bool,
    pub scope: SemanticIndexScope,
}

impl Default for SemanticRuntimeConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl SemanticRuntimeConfig {
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            embedding_model: None,
            reranker_model: None,
            model_cache_dir: None,
            index_cache_dir: None,
            rerank: true,
            mock: false,
            scope: SemanticIndexScope::default(),
        }
    }

    #[must_use]
    pub fn mock() -> Self {
        Self {
            enabled: true,
            embedding_model: Some("mock".to_string()),
            reranker_model: Some("mock".to_string()),
            model_cache_dir: None,
            index_cache_dir: None,
            rerank: true,
            mock: true,
            scope: SemanticIndexScope::default(),
        }
    }

    pub fn build_index(&self) -> Result<Option<Arc<SemanticIndex>>, NerveError> {
        self.build_index_with_roots(&[])
    }

    pub fn build_index_for_roots(
        &self,
        roots: &[RootRef],
    ) -> Result<Option<Arc<SemanticIndex>>, NerveError> {
        self.build_index_with_roots(roots)
    }

    pub(super) fn build_index_with_roots(
        &self,
        roots: &[RootRef],
    ) -> Result<Option<Arc<SemanticIndex>>, NerveError> {
        if !self.enabled {
            return Ok(None);
        }
        let embedding_model_id = embedding_model_id(self.embedding_model.as_deref());
        let embedding_dimension = if self.mock || self.embedding_model.as_deref() == Some("mock") {
            MockEmbeddingBackend::default().dimension()
        } else {
            embedding_dimension(&parse_embedding_model(self.embedding_model.as_deref())?)
        };
        let config = SemanticIndexConfig {
            rerank: self.rerank,
            persistence: semantic_persistence_config(
                self.index_cache_dir.as_deref(),
                roots,
                &embedding_model_id,
                embedding_dimension,
                &self.scope,
            )?,
            scope: self.scope.clone(),
            ..SemanticIndexConfig::default()
        };
        if self.mock || self.embedding_model.as_deref() == Some("mock") {
            return Ok(Some(Arc::new(SemanticIndex::mock_with_config(config))));
        }
        let cache_dir = semantic_model_cache_dir(self.model_cache_dir.as_deref());
        let embedding = Arc::new(FastembedEmbeddingBackend::new(
            parse_embedding_model(self.embedding_model.as_deref())?,
            cache_dir.clone(),
        ));
        let reranker = if self.rerank && self.reranker_model.as_deref() != Some("none") {
            Some(Arc::new(FastembedRerankerBackend::new(
                parse_reranker_model(self.reranker_model.as_deref())?,
                cache_dir,
            )) as Arc<dyn RerankerBackend>)
        } else {
            None
        };
        Ok(Some(Arc::new(SemanticIndex::new(
            config, embedding, reranker,
        ))))
    }
}

impl Default for SemanticIndexConfig {
    fn default() -> Self {
        Self {
            candidates: DEFAULT_CANDIDATES,
            rerank_limit: DEFAULT_RERANK_LIMIT,
            rerank: true,
            persistence: None,
            scope: SemanticIndexScope::default(),
            compaction_tombstone_ratio: TOMBSTONE_RATIO_THRESHOLD,
            compaction_tombstone_count: TOMBSTONE_COUNT_THRESHOLD,
        }
    }
}
