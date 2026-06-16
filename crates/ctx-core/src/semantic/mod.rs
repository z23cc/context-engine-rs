//! Feature-gated semantic + hybrid retrieval.

pub(crate) mod chunk;

use self::chunk::{
    CHUNKER_VERSION, ChunkBuild, SemanticChunk, build_chunks, build_chunks_for_entries,
};
use crate::{
    cancel::CancelToken,
    models::{
        CatalogEntry, CtxError, Diagnostic, RootRef, SemanticSearchMode, SemanticSearchRequest,
        SemanticSearchResponse, SemanticSearchResult, SemanticSearchTotals,
    },
    port::{CatalogProvider, FileSignature},
    ranking::{tokenize_query, tokenize_text},
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
    sync::{Arc, Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

const DEFAULT_CANDIDATES: usize = 100;
const DEFAULT_RERANK_LIMIT: usize = 100;
const RRF_K: f64 = 60.0;
const HNSW_MAX_CONN: usize = 16;
const HNSW_MAX_LAYER: usize = 16;
const HNSW_EF_CONSTRUCTION: usize = 200;
const HNSW_EF_SEARCH: usize = 128;
const SCHEMA_VERSION: u32 = 1;
const TOMBSTONE_RATIO_THRESHOLD: f64 = 0.20;
const TOMBSTONE_COUNT_THRESHOLD: usize = 10_000;

pub trait EmbeddingBackend: Send + Sync {
    fn dimension(&self) -> usize;
    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError>;
    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CtxError>;
}

pub trait RerankerBackend: Send + Sync {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, CtxError>;
}

#[derive(Clone)]
pub struct SemanticIndexConfig {
    pub candidates: usize,
    pub rerank_limit: usize,
    pub rerank: bool,
    pub persistence: Option<SemanticPersistenceConfig>,
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
        }
    }

    pub fn build_index(&self) -> Result<Option<Arc<SemanticIndex>>, CtxError> {
        self.build_index_with_roots(&[])
    }

    pub fn build_index_for_roots(
        &self,
        roots: &[RootRef],
    ) -> Result<Option<Arc<SemanticIndex>>, CtxError> {
        self.build_index_with_roots(roots)
    }

    fn build_index_with_roots(
        &self,
        roots: &[RootRef],
    ) -> Result<Option<Arc<SemanticIndex>>, CtxError> {
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
            )?,
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
            compaction_tombstone_ratio: TOMBSTONE_RATIO_THRESHOLD,
            compaction_tombstone_count: TOMBSTONE_COUNT_THRESHOLD,
        }
    }
}

pub struct SemanticIndex {
    config: SemanticIndexConfig,
    embedding: Arc<dyn EmbeddingBackend>,
    reranker: Option<Arc<dyn RerankerBackend>>,
    built: RwLock<Option<Arc<BuiltSemanticIndex>>>,
    build_lock: Mutex<()>,
}

impl fmt::Debug for SemanticIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SemanticIndex")
            .field("config", &"SemanticIndexConfig")
            .finish_non_exhaustive()
    }
}

impl SemanticIndex {
    #[must_use]
    pub fn new(
        config: SemanticIndexConfig,
        embedding: Arc<dyn EmbeddingBackend>,
        reranker: Option<Arc<dyn RerankerBackend>>,
    ) -> Self {
        Self {
            config,
            embedding,
            reranker,
            built: RwLock::new(None),
            build_lock: Mutex::new(()),
        }
    }

    #[must_use]
    pub fn mock() -> Self {
        Self::mock_with_config(SemanticIndexConfig::default())
    }

    #[must_use]
    pub fn mock_with_config(config: SemanticIndexConfig) -> Self {
        Self::new(
            config,
            Arc::new(MockEmbeddingBackend::default()),
            Some(Arc::new(MockRerankerBackend)),
        )
    }

    pub fn invalidate(&self) {
        *self.built.write().expect("semantic index lock") = None;
    }

    pub fn search<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        request: &SemanticSearchRequest,
        cancel: &CancelToken,
    ) -> Result<SemanticSearchResponse, CtxError> {
        cancel.check_cancelled()?;
        let built = self.ensure_built(provider, snapshot, cancel)?;
        let max_results = request.max_results.max(1);
        let candidate_limit = self.config.candidates.max(max_results);
        let query_vector = self.embedding.embed_query(&request.query)?;
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
            && self.config.rerank
            && let Some(reranker) = &self.reranker
        {
            let rerank_limit = self.config.rerank_limit.min(scored.len());
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
            results,
            diagnostics: built.diagnostics.clone(),
            totals: SemanticSearchTotals {
                scanned_files: snapshot.entries.len(),
                chunks: built.chunks.len(),
                dense_candidates: dense.len(),
                bm25_candidates: bm25.len(),
                fused_candidates: scored.len(),
                reranked,
            },
        })
    }

    fn ensure_built<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        cancel: &CancelToken,
    ) -> Result<Arc<BuiltSemanticIndex>, CtxError> {
        if self.config.persistence.is_some() {
            let state = SnapshotFileState::from_snapshot(provider, snapshot)?;
            if let Some(cached) = self.built.read().expect("semantic index lock").as_ref()
                && cached.manifest_fingerprint == state.fingerprint
            {
                return Ok(Arc::clone(cached));
            }
            let _guard = self.build_lock.lock().expect("semantic build lock");
            if let Some(cached) = self.built.read().expect("semantic index lock").as_ref()
                && cached.manifest_fingerprint == state.fingerprint
            {
                return Ok(Arc::clone(cached));
            }
            let built = Arc::new(self.build_with_persistence(provider, snapshot, &state, cancel)?);
            *self.built.write().expect("semantic index lock") = Some(Arc::clone(&built));
            return Ok(built);
        }

        let chunk_build = build_chunks(provider, snapshot, cancel)?;
        if let Some(cached) = self.built.read().expect("semantic index lock").as_ref()
            && cached.manifest_fingerprint == chunk_build.manifest.fingerprint
        {
            return Ok(Arc::clone(cached));
        }

        let _guard = self.build_lock.lock().expect("semantic build lock");
        if let Some(cached) = self.built.read().expect("semantic index lock").as_ref()
            && cached.manifest_fingerprint == chunk_build.manifest.fingerprint
        {
            return Ok(Arc::clone(cached));
        }
        let built = Arc::new(self.build_from_chunks(chunk_build, cancel)?);
        *self.built.write().expect("semantic index lock") = Some(Arc::clone(&built));
        Ok(built)
    }

    fn build_from_chunks(
        &self,
        chunk_build: ChunkBuild,
        cancel: &CancelToken,
    ) -> Result<BuiltSemanticIndex, CtxError> {
        let texts: Vec<String> = chunk_build
            .chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect();
        let vectors = self.embed_chunk_texts(&texts)?;
        cancel.check_cancelled()?;
        Self::built_from_active(
            chunk_build.manifest.fingerprint,
            chunk_build.chunks,
            vectors,
            chunk_build.diagnostics,
            self.embedding.dimension(),
        )
    }

    fn build_with_persistence<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        state: &SnapshotFileState,
        cancel: &CancelToken,
    ) -> Result<BuiltSemanticIndex, CtxError> {
        let persistence = self
            .config
            .persistence
            .as_ref()
            .expect("persistence config checked");
        let loaded = match load_persisted_index(persistence) {
            Ok(Some(index)) => Some(index),
            Ok(None) => None,
            Err(_) => {
                let _ = clean_workspace_cache(persistence);
                None
            }
        };

        let mut records = loaded
            .as_ref()
            .map(|index| index.records.clone())
            .unwrap_or_default();
        let mut files = BTreeMap::new();
        let mut old_files = loaded
            .as_ref()
            .map(|index| index.files.clone())
            .unwrap_or_default();
        let mut changed_entries = Vec::new();
        let mut current_keys = HashSet::new();

        for file in &state.files {
            current_keys.insert(file.key.clone());
            if let Some(old_file) = old_files.remove(&file.key)
                && old_file.signature == file.signature
            {
                files.insert(file.key.clone(), old_file);
                continue;
            }
            if let Some(old_file) = loaded.as_ref().and_then(|index| index.files.get(&file.key)) {
                tombstone_chunks(&mut records, &old_file.file_key, &old_file.chunk_ids);
            }
            changed_entries.push(file.entry.clone());
        }

        for (_, removed) in old_files {
            if !current_keys.contains(&removed.file_key) {
                tombstone_chunks(&mut records, &removed.file_key, &removed.chunk_ids);
            }
        }

        let mut diagnostics = Vec::new();
        for entry in changed_entries {
            cancel.check_cancelled()?;
            let build = build_chunks_for_entries(provider, &[&entry], snapshot.generation, cancel)?;
            diagnostics.extend(build.diagnostics);
            let texts: Vec<String> = build
                .chunks
                .iter()
                .map(|chunk| chunk.text.clone())
                .collect();
            let embeddings = self.embed_chunk_texts(&texts)?;
            let file_key = file_key(&entry.root_id, &entry.rel_path);
            let chunk_ids = build
                .chunks
                .iter()
                .map(|chunk| chunk.id.clone())
                .collect::<Vec<_>>();
            for (chunk, embedding) in build.chunks.into_iter().zip(embeddings) {
                records.push(SemanticChunkRecord {
                    chunk,
                    embedding,
                    active: true,
                    file_key: file_key.clone(),
                });
            }
            let signature = state
                .files
                .iter()
                .find(|file| file.key == file_key)
                .map(|file| file.signature.clone())
                .unwrap_or(PersistedFileSignature {
                    modified_unix_nanos: None,
                    size: entry.size,
                });
            files.insert(
                file_key.clone(),
                PersistedFileRecord {
                    file_key,
                    root_id: entry.root_id,
                    rel_path: entry.rel_path,
                    signature,
                    chunk_ids,
                },
            );
        }

        let tombstones = records.iter().filter(|record| !record.active).count();
        if should_compact(records.len(), tombstones, &self.config) {
            records.retain(|record| record.active);
        }
        let built =
            self.built_from_records(state.fingerprint.clone(), &records, diagnostics.clone())?;
        if let Err(err) =
            save_persisted_index(persistence, &records, &files, diagnostics, &built.ann)
        {
            let mut with_diagnostic = built;
            with_diagnostic.diagnostics.push(Diagnostic {
                path: None,
                message: format!("semantic cache write failed; using in-memory index: {err}"),
            });
            return Ok(with_diagnostic);
        }
        Ok(built)
    }

    fn embed_chunk_texts(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.embedding.embed_documents(texts)?;
        if vectors.len() != texts.len() {
            return Err(CtxError::Semantic(format!(
                "embedding backend returned {} vectors for {} chunks",
                vectors.len(),
                texts.len()
            )));
        }
        Ok(vectors)
    }

    fn built_from_records(
        &self,
        fingerprint: String,
        records: &[SemanticChunkRecord],
        diagnostics: Vec<Diagnostic>,
    ) -> Result<BuiltSemanticIndex, CtxError> {
        let active: Vec<_> = records.iter().filter(|record| record.active).collect();
        let chunks = active
            .iter()
            .map(|record| record.chunk.clone())
            .collect::<Vec<_>>();
        let vectors = active
            .iter()
            .map(|record| record.embedding.clone())
            .collect::<Vec<_>>();
        Self::built_from_active(
            fingerprint,
            chunks,
            vectors,
            diagnostics,
            self.embedding.dimension(),
        )
    }

    fn built_from_active(
        fingerprint: String,
        chunks: Vec<SemanticChunk>,
        vectors: Vec<Vec<f32>>,
        diagnostics: Vec<Diagnostic>,
        dimension: usize,
    ) -> Result<BuiltSemanticIndex, CtxError> {
        let ann = DenseAnn::new(vectors, dimension)?;
        let bm25 = ChunkBm25::new(&chunks);
        Ok(BuiltSemanticIndex {
            manifest_fingerprint: fingerprint,
            chunks,
            diagnostics,
            ann,
            bm25,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SemanticChunkRecord {
    chunk: SemanticChunk,
    embedding: Vec<f32>,
    active: bool,
    file_key: String,
}

struct BuiltSemanticIndex {
    manifest_fingerprint: String,
    chunks: Vec<SemanticChunk>,
    diagnostics: Vec<Diagnostic>,
    ann: DenseAnn,
    bm25: ChunkBm25,
}

#[derive(Clone, Debug)]
struct SnapshotFileState {
    files: Vec<SnapshotFileRecord>,
    fingerprint: String,
}

#[derive(Clone, Debug)]
struct SnapshotFileRecord {
    key: String,
    entry: CatalogEntry,
    signature: PersistedFileSignature,
}

impl SnapshotFileState {
    fn from_snapshot<P: CatalogProvider + Sync>(
        provider: &P,
        snapshot: &CatalogSnapshot,
    ) -> Result<Self, CtxError> {
        let mut files = Vec::with_capacity(snapshot.entries.len());
        let mut hasher = Sha256::new();
        for entry in &snapshot.entries {
            let signature = provider
                .file_signature(Path::new(&entry.abs_path))?
                .map(PersistedFileSignature::from)
                .unwrap_or(PersistedFileSignature {
                    modified_unix_nanos: None,
                    size: entry.size,
                });
            let key = file_key(&entry.root_id, &entry.rel_path);
            hasher.update(key.as_bytes());
            hasher.update(signature.size.to_le_bytes());
            hasher.update(
                signature
                    .modified_unix_nanos
                    .unwrap_or_default()
                    .to_le_bytes(),
            );
            files.push(SnapshotFileRecord {
                key,
                entry: entry.clone(),
                signature,
            });
        }
        Ok(Self {
            files,
            fingerprint: format!("{:x}", hasher.finalize()),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedFileSignature {
    modified_unix_nanos: Option<i128>,
    size: u64,
}

impl From<FileSignature> for PersistedFileSignature {
    fn from(signature: FileSignature) -> Self {
        Self {
            modified_unix_nanos: signature.modified.and_then(system_time_to_unix_nanos),
            size: signature.size,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedFileRecord {
    file_key: String,
    root_id: String,
    rel_path: String,
    signature: PersistedFileSignature,
    chunk_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedCurrent {
    generation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedManifest {
    schema_version: u32,
    chunker_version: u32,
    workspace_key: String,
    embedding_model_id: String,
    embedding_dimension: usize,
    roots: Vec<String>,
    active_count: usize,
    tombstone_count: usize,
    files: Vec<PersistedFileRecord>,
    chunks: Vec<PersistedChunkRecord>,
    diagnostics: Vec<Diagnostic>,
    embeddings: EmbeddingArtifact,
    ann: AnnArtifact,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedChunkRecord {
    chunk: SemanticChunk,
    file_key: String,
    embedding_row: usize,
    active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EmbeddingArtifact {
    path: String,
    rows: usize,
    dimension: usize,
    sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AnnArtifact {
    path: String,
    rebuilt_from_embeddings: bool,
    active_count: usize,
}

#[derive(Clone, Debug)]
struct LoadedPersistedIndex {
    files: BTreeMap<String, PersistedFileRecord>,
    records: Vec<SemanticChunkRecord>,
}

fn semantic_persistence_config(
    configured_dir: Option<&Path>,
    roots: &[RootRef],
    embedding_model_id: &str,
    embedding_dimension: usize,
) -> Result<Option<SemanticPersistenceConfig>, CtxError> {
    let cache_base_dir = configured_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_semantic_cache_dir);
    let canonical_roots = roots
        .iter()
        .map(|root| root.path.clone())
        .collect::<Vec<_>>();
    let workspace_key = workspace_key(&canonical_roots, embedding_model_id, embedding_dimension);
    Ok(Some(SemanticPersistenceConfig {
        cache_base_dir,
        workspace_key,
        roots: canonical_roots,
        embedding_model_id: embedding_model_id.to_string(),
        embedding_dimension,
    }))
}

fn workspace_key(
    roots: &[PathBuf],
    embedding_model_id: &str,
    embedding_dimension: usize,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    hasher.update(CHUNKER_VERSION.to_le_bytes());
    hasher.update(embedding_model_id.as_bytes());
    hasher.update(embedding_dimension.to_le_bytes());
    for root in roots {
        hasher.update(root.to_string_lossy().as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn default_semantic_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("context-engine-rs/semantic");
    }
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(dir).join("context-engine-rs/semantic");
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        {
            return home.join("Library/Caches/context-engine-rs/semantic");
        }
        #[cfg(not(target_os = "macos"))]
        {
            return home.join(".cache/context-engine-rs/semantic");
        }
    }
    std::env::temp_dir().join("context-engine-rs/semantic")
}

fn cache_workspace_dir(config: &SemanticPersistenceConfig) -> PathBuf {
    config.cache_base_dir.join(&config.workspace_key)
}

fn current_path(config: &SemanticPersistenceConfig) -> PathBuf {
    cache_workspace_dir(config).join("current.json")
}

fn generation_dir(config: &SemanticPersistenceConfig, generation: &str) -> PathBuf {
    cache_workspace_dir(config)
        .join("generations")
        .join(generation)
}

fn load_persisted_index(
    config: &SemanticPersistenceConfig,
) -> Result<Option<LoadedPersistedIndex>, CtxError> {
    let current_path = current_path(config);
    if !current_path.exists() {
        return Ok(None);
    }
    let current: PersistedCurrent = read_json(&current_path)?;
    let dir = generation_dir(config, &current.generation);
    let manifest_path = dir.join("manifest.json");
    let manifest: PersistedManifest = read_json(&manifest_path)?;
    validate_manifest(config, &manifest)?;
    let embedding_path = dir.join(&manifest.embeddings.path);
    let embeddings = read_embeddings(
        &embedding_path,
        manifest.embeddings.rows,
        manifest.embeddings.dimension,
    )?;
    let bytes = fs::read(&embedding_path).map_err(|err| CtxError::io(&embedding_path, err))?;
    if sha256_hex(&bytes) != manifest.embeddings.sha256 {
        return Err(CtxError::Semantic(
            "semantic embedding artifact checksum mismatch".into(),
        ));
    }
    if embeddings.len() != manifest.chunks.len() {
        return Err(CtxError::Semantic(
            "semantic embedding/chunk row mismatch".into(),
        ));
    }
    let mut records = Vec::with_capacity(manifest.chunks.len());
    for chunk in manifest.chunks {
        let embedding = embeddings
            .get(chunk.embedding_row)
            .cloned()
            .ok_or_else(|| CtxError::Semantic("semantic embedding row out of range".into()))?;
        records.push(SemanticChunkRecord {
            chunk: chunk.chunk,
            embedding,
            active: chunk.active,
            file_key: chunk.file_key,
        });
    }
    Ok(Some(LoadedPersistedIndex {
        files: manifest
            .files
            .into_iter()
            .map(|file| (file.file_key.clone(), file))
            .collect(),
        records,
    }))
}

fn save_persisted_index(
    config: &SemanticPersistenceConfig,
    records: &[SemanticChunkRecord],
    files: &BTreeMap<String, PersistedFileRecord>,
    diagnostics: Vec<Diagnostic>,
    ann: &DenseAnn,
) -> Result<(), CtxError> {
    let generation = generation_id();
    let dir = generation_dir(config, &generation);
    fs::create_dir_all(&dir).map_err(|err| CtxError::io(&dir, err))?;
    let embeddings_bytes = embeddings_to_bytes(records);
    let embeddings_sha = sha256_hex(&embeddings_bytes);
    let embeddings_path = dir.join("embeddings.f32");
    write_synced(&embeddings_path, &embeddings_bytes)?;
    let ann_basename = ann
        .dump(&dir, "ann")
        .unwrap_or_else(|_| Some("ann".to_string()))
        .unwrap_or_else(|| "ann".to_string());
    let ann_artifact = AnnArtifact {
        path: ann_basename,
        rebuilt_from_embeddings: true,
        active_count: records.iter().filter(|record| record.active).count(),
    };
    write_synced(
        &dir.join("ann.meta.json"),
        serde_json::to_vec_pretty(&ann_artifact)
            .map_err(|err| {
                CtxError::Semantic(format!("semantic ann metadata encode failed: {err}"))
            })?
            .as_slice(),
    )?;
    let chunks = records
        .iter()
        .enumerate()
        .map(|(row, record)| PersistedChunkRecord {
            chunk: record.chunk.clone(),
            file_key: record.file_key.clone(),
            embedding_row: row,
            active: record.active,
        })
        .collect::<Vec<_>>();
    let manifest = PersistedManifest {
        schema_version: SCHEMA_VERSION,
        chunker_version: CHUNKER_VERSION,
        workspace_key: config.workspace_key.clone(),
        embedding_model_id: config.embedding_model_id.clone(),
        embedding_dimension: config.embedding_dimension,
        roots: config
            .roots
            .iter()
            .map(|root| root.to_string_lossy().replace('\\', "/"))
            .collect(),
        active_count: records.iter().filter(|record| record.active).count(),
        tombstone_count: records.iter().filter(|record| !record.active).count(),
        files: files.values().cloned().collect(),
        chunks,
        diagnostics,
        embeddings: EmbeddingArtifact {
            path: "embeddings.f32".to_string(),
            rows: records.len(),
            dimension: config.embedding_dimension,
            sha256: embeddings_sha,
        },
        ann: ann_artifact,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|err| CtxError::Semantic(format!("semantic manifest encode failed: {err}")))?;
    write_synced(&dir.join("manifest.json"), &manifest_bytes)?;
    sync_dir(&dir)?;
    let current_bytes = serde_json::to_vec_pretty(&PersistedCurrent { generation })
        .map_err(|err| CtxError::Semantic(format!("semantic current encode failed: {err}")))?;
    write_atomic(&current_path(config), &current_bytes)
}

fn validate_manifest(
    config: &SemanticPersistenceConfig,
    manifest: &PersistedManifest,
) -> Result<(), CtxError> {
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.chunker_version != CHUNKER_VERSION
        || manifest.workspace_key != config.workspace_key
        || manifest.embedding_model_id != config.embedding_model_id
        || manifest.embedding_dimension != config.embedding_dimension
    {
        return Err(CtxError::Semantic(
            "semantic cache manifest is incompatible".into(),
        ));
    }
    Ok(())
}

fn clean_workspace_cache(config: &SemanticPersistenceConfig) -> Result<(), CtxError> {
    let dir = cache_workspace_dir(config);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|err| CtxError::io(&dir, err))?;
    }
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, CtxError> {
    let bytes = fs::read(path).map_err(|err| CtxError::io(path, err))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| CtxError::Semantic(format!("semantic cache JSON decode failed: {err}")))
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), CtxError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| CtxError::io(parent, err))?;
    }
    let mut file = File::create(path).map_err(|err| CtxError::io(path, err))?;
    file.write_all(bytes)
        .map_err(|err| CtxError::io(path, err))?;
    file.sync_all().map_err(|err| CtxError::io(path, err))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CtxError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|err| CtxError::io(parent, err))?;
    let tmp = parent.join(format!(".{}.tmp", generation_id()));
    write_synced(&tmp, bytes)?;
    rename_replace(&tmp, path)?;
    sync_dir(parent)
}

#[cfg(not(windows))]
fn rename_replace(from: &Path, to: &Path) -> Result<(), CtxError> {
    fs::rename(from, to).map_err(|err| CtxError::io(to, err))
}

#[cfg(windows)]
fn rename_replace(from: &Path, to: &Path) -> Result<(), CtxError> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    fn wide(path: &OsStr) -> Vec<u16> {
        path.encode_wide().chain(std::iter::once(0)).collect()
    }

    let error_path = to.to_path_buf();
    let from = wide(from.as_os_str());
    let to = wide(to.as_os_str());
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(CtxError::io(error_path, std::io::Error::last_os_error()));
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<(), CtxError> {
    match File::open(path).and_then(|file| file.sync_all()) {
        Ok(()) => Ok(()),
        Err(_) => Ok(()),
    }
}

fn embeddings_to_bytes(records: &[SemanticChunkRecord]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for record in records {
        for value in &record.embedding {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

fn read_embeddings(path: &Path, rows: usize, dimension: usize) -> Result<Vec<Vec<f32>>, CtxError> {
    let expected = rows
        .checked_mul(dimension)
        .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| CtxError::Semantic("semantic embedding artifact size overflow".into()))?;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|err| CtxError::io(path, err))?;
    if bytes.len() != expected {
        return Err(CtxError::Semantic(format!(
            "semantic embedding artifact has {} bytes, expected {expected}",
            bytes.len()
        )));
    }
    let mut vectors = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut vector = Vec::with_capacity(dimension);
        for col in 0..dimension {
            let offset = (row * dimension + col) * std::mem::size_of::<f32>();
            vector.push(f32::from_le_bytes(
                bytes[offset..offset + 4]
                    .try_into()
                    .expect("embedding f32 byte slice"),
            ));
        }
        vectors.push(vector);
    }
    Ok(vectors)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn generation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

fn system_time_to_unix_nanos(time: SystemTime) -> Option<i128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i128::try_from(duration.as_nanos()).ok())
}

fn file_key(root_id: &str, rel_path: &str) -> String {
    format!("{root_id}\0{rel_path}")
}

fn tombstone_chunks(records: &mut [SemanticChunkRecord], file_key: &str, chunk_ids: &[String]) {
    let chunk_ids: HashSet<&str> = chunk_ids.iter().map(String::as_str).collect();
    for record in records {
        if record.file_key == file_key && chunk_ids.contains(record.chunk.id.as_str()) {
            record.active = false;
        }
    }
}

fn should_compact(total_records: usize, tombstones: usize, config: &SemanticIndexConfig) -> bool {
    tombstones > 0
        && (tombstones >= config.compaction_tombstone_count
            || (tombstones as f64 / total_records.max(1) as f64)
                >= config.compaction_tombstone_ratio)
}

fn embedding_model_id(model: Option<&str>) -> String {
    match model.unwrap_or("jina-embeddings-v2-base-code") {
        "jinaai/jina-embeddings-v2-base-code" => "jina-embeddings-v2-base-code".to_string(),
        "BAAI/bge-small-en-v1.5" => "bge-small-en-v1.5".to_string(),
        other => other.to_string(),
    }
}

struct DenseAnn {
    hnsw: Option<Hnsw<'static, f32, DistCosine>>,
}

impl DenseAnn {
    fn new(vectors: Vec<Vec<f32>>, dimension: usize) -> Result<Self, CtxError> {
        if vectors.is_empty() {
            return Ok(Self { hnsw: None });
        }
        for vector in &vectors {
            if vector.len() != dimension {
                return Err(CtxError::Semantic(format!(
                    "embedding dimension mismatch: expected {dimension}, got {}",
                    vector.len()
                )));
            }
        }
        let hnsw = Hnsw::<f32, DistCosine>::new(
            HNSW_MAX_CONN,
            vectors.len().max(1),
            HNSW_MAX_LAYER,
            HNSW_EF_CONSTRUCTION,
            DistCosine {},
        );
        for (idx, vector) in vectors.iter().enumerate() {
            hnsw.insert((vector.as_slice(), idx));
        }
        Ok(Self { hnsw: Some(hnsw) })
    }

    fn search(&self, query: &[f32], limit: usize) -> Vec<(usize, f64)> {
        let Some(hnsw) = &self.hnsw else {
            return Vec::new();
        };
        hnsw.search(query, limit, HNSW_EF_SEARCH)
            .into_iter()
            .map(|neighbour| (neighbour.d_id, 1.0 / (1.0 + neighbour.distance as f64)))
            .collect()
    }

    fn dump(&self, dir: &Path, basename: &str) -> Result<Option<String>, CtxError> {
        let Some(hnsw) = &self.hnsw else {
            return Ok(None);
        };
        hnsw.file_dump(dir, basename)
            .map(Some)
            .map_err(|err| CtxError::Semantic(format!("semantic ANN dump failed: {err}")))
    }
}

#[derive(Debug, Clone)]
struct ChunkBm25 {
    docs: Vec<ChunkBm25Doc>,
    document_frequencies: HashMap<String, usize>,
    avg_doc_len: f64,
}

#[derive(Debug, Clone)]
struct ChunkBm25Doc {
    chunk_idx: usize,
    doc_len: usize,
    term_frequencies: HashMap<String, usize>,
}

impl ChunkBm25 {
    fn new(chunks: &[SemanticChunk]) -> Self {
        let mut docs = Vec::with_capacity(chunks.len());
        let mut document_frequencies: HashMap<String, usize> = HashMap::new();
        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            let mut term_frequencies = HashMap::new();
            for token in tokenize_text(&chunk.text, false) {
                *term_frequencies.entry(token).or_insert(0) += 1;
            }
            for term in term_frequencies.keys() {
                *document_frequencies.entry(term.clone()).or_insert(0) += 1;
            }
            docs.push(ChunkBm25Doc {
                chunk_idx,
                doc_len: term_frequencies.values().sum::<usize>().max(1),
                term_frequencies,
            });
        }
        let avg_doc_len = if docs.is_empty() {
            1.0
        } else {
            docs.iter().map(|doc| doc.doc_len as f64).sum::<f64>() / docs.len() as f64
        };
        Self {
            docs,
            document_frequencies,
            avg_doc_len,
        }
    }

    fn search(&self, query: &str, limit: usize) -> Vec<(usize, f64)> {
        let terms = tokenize_query(query, false);
        if terms.is_empty() || self.docs.is_empty() {
            return Vec::new();
        }
        let doc_count = self.docs.len() as f64;
        let mut scores: Vec<(usize, f64)> = self
            .docs
            .iter()
            .filter_map(|doc| {
                let mut score = 0.0;
                for term in &terms {
                    let tf = doc.term_frequencies.get(term).copied().unwrap_or(0) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = self.document_frequencies.get(term).copied().unwrap_or(0) as f64;
                    let idf = (1.0 + (doc_count - df + 0.5) / (df + 0.5)).ln();
                    let length_norm = 1.0 - 0.75 + 0.75 * (doc.doc_len as f64 / self.avg_doc_len);
                    let saturated_tf = (tf * (1.2 + 1.0)) / (tf + 1.2 * length_norm);
                    score += idf * saturated_tf;
                }
                (score > 0.0).then_some((doc.chunk_idx, score))
            })
            .collect();
        scores.sort_by(|left, right| rank_cmp(left.1, right.1).then_with(|| left.0.cmp(&right.0)));
        scores.truncate(limit);
        scores
    }
}

struct FastembedEmbeddingBackend {
    model: EmbeddingModel,
    dimension: usize,
    cache_dir: PathBuf,
    inner: Mutex<Option<TextEmbedding>>,
}

impl FastembedEmbeddingBackend {
    fn new(model: EmbeddingModel, cache_dir: PathBuf) -> Self {
        Self {
            dimension: embedding_dimension(&model),
            model,
            cache_dir,
            inner: Mutex::new(None),
        }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        let mut guard = self.inner.lock().expect("fastembed embedding lock");
        if guard.is_none() {
            let options = TextInitOptions::new(self.model.clone())
                .with_cache_dir(self.cache_dir.clone())
                .with_show_download_progress(false);
            *guard = Some(TextEmbedding::try_new(options).map_err(|err| {
                CtxError::Semantic(format!("embedding model init failed: {err}"))
            })?);
        }
        guard
            .as_mut()
            .expect("embedding initialized")
            .embed(texts, None)
            .map_err(|err| CtxError::Semantic(format!("embedding failed: {err}")))
    }
}

impl EmbeddingBackend for FastembedEmbeddingBackend {
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        self.embed(texts)
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CtxError> {
        let embeddings = self.embed(&[query.to_string()])?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| CtxError::Semantic("embedding backend returned no query vector".into()))
    }
}

struct FastembedRerankerBackend {
    model: RerankerModel,
    cache_dir: PathBuf,
    inner: Mutex<Option<TextRerank>>,
}

impl FastembedRerankerBackend {
    fn new(model: RerankerModel, cache_dir: PathBuf) -> Self {
        Self {
            model,
            cache_dir,
            inner: Mutex::new(None),
        }
    }
}

impl RerankerBackend for FastembedRerankerBackend {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, CtxError> {
        let mut guard = self.inner.lock().expect("fastembed reranker lock");
        if guard.is_none() {
            let options = RerankInitOptions::new(self.model.clone())
                .with_cache_dir(self.cache_dir.clone())
                .with_show_download_progress(false);
            *guard =
                Some(TextRerank::try_new(options).map_err(|err| {
                    CtxError::Semantic(format!("reranker model init failed: {err}"))
                })?);
        }
        let ranked = guard
            .as_mut()
            .expect("reranker initialized")
            .rerank(query.to_string(), documents, false, None)
            .map_err(|err| CtxError::Semantic(format!("rerank failed: {err}")))?;
        let mut scores = vec![0.0; documents.len()];
        for result in ranked {
            if result.index < scores.len() {
                scores[result.index] = result.score;
            }
        }
        Ok(scores)
    }
}

fn parse_embedding_model(model: Option<&str>) -> Result<EmbeddingModel, CtxError> {
    match model.unwrap_or("jina-embeddings-v2-base-code") {
        "jina-embeddings-v2-base-code" | "jinaai/jina-embeddings-v2-base-code" => {
            Ok(EmbeddingModel::JinaEmbeddingsV2BaseCode)
        }
        "bge-small-en-v1.5" | "BAAI/bge-small-en-v1.5" => Ok(EmbeddingModel::BGESmallENV15),
        other => Err(CtxError::Semantic(format!(
            "unsupported embedding model for semantic_search: {other}"
        ))),
    }
}

fn parse_reranker_model(model: Option<&str>) -> Result<RerankerModel, CtxError> {
    match model.unwrap_or("bge-reranker-base") {
        "bge-reranker-base" | "BAAI/bge-reranker-base" => Ok(RerankerModel::BGERerankerBase),
        other => Err(CtxError::Semantic(format!(
            "unsupported reranker model for semantic_search: {other}"
        ))),
    }
}

fn embedding_dimension(model: &EmbeddingModel) -> usize {
    match model {
        EmbeddingModel::JinaEmbeddingsV2BaseCode => 768,
        EmbeddingModel::BGESmallENV15 => 384,
        _ => 768,
    }
}

fn semantic_model_cache_dir(configured: Option<&Path>) -> PathBuf {
    configured.map_or_else(
        || {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".fastembed_cache")
        },
        Path::to_path_buf,
    )
}

#[derive(Default)]
pub struct MockEmbeddingBackend {
    dimension: usize,
}

impl MockEmbeddingBackend {
    #[must_use]
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }

    fn embed_text(&self, text: &str) -> Vec<f32> {
        let dimension = self.dimension();
        let mut vector = vec![0.0; dimension];
        for token in tokenize_text(text, false) {
            let idx = stable_bucket(&token, dimension);
            vector[idx] += 1.0;
        }
        normalize(&mut vector);
        vector
    }
}

impl EmbeddingBackend for MockEmbeddingBackend {
    fn dimension(&self) -> usize {
        self.dimension.max(32)
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
        Ok(texts.iter().map(|text| self.embed_text(text)).collect())
    }

    fn embed_query(&self, query: &str) -> Result<Vec<f32>, CtxError> {
        Ok(self.embed_text(query))
    }
}

pub struct MockRerankerBackend;

impl RerankerBackend for MockRerankerBackend {
    fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, CtxError> {
        let query_terms: HashSet<String> = tokenize_text(query, false).into_iter().collect();
        Ok(documents
            .iter()
            .map(|doc| {
                let doc_terms: HashSet<String> = tokenize_text(doc, false).into_iter().collect();
                query_terms.intersection(&doc_terms).count() as f32
            })
            .collect())
    }
}

fn rrf_fuse(dense: &[(usize, f64)], bm25: &[(usize, f64)]) -> Vec<(usize, f64)> {
    let mut scores: HashMap<usize, f64> = HashMap::new();
    for ranking in [dense, bm25] {
        for (rank, (idx, _)) in ranking.iter().enumerate() {
            *scores.entry(*idx).or_insert(0.0) += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    scores.into_iter().collect()
}

fn rank_cmp(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

fn chunk_cmp(left: &SemanticChunk, right: &SemanticChunk) -> Ordering {
    left.path
        .cmp(&right.path)
        .then_with(|| left.line_start.cmp(&right.line_start))
        .then_with(|| left.id.cmp(&right.id))
}

fn chunk_to_result(chunk: &SemanticChunk, score: f64) -> SemanticSearchResult {
    SemanticSearchResult {
        root_id: chunk.root_id.clone(),
        path: chunk.path.clone(),
        display_path: chunk.display_path.clone(),
        score,
        line_start: chunk.line_start,
        line_end: chunk.line_end,
        symbol: chunk.symbol.clone(),
        signature: chunk.signature.clone(),
        snippet: chunk.text.clone(),
    }
}

fn stable_bucket(token: &str, dimension: usize) -> usize {
    let mut hash = 1469598103934665603u64;
    for byte in token.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    (hash as usize) % dimension
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, HostFile, MemoryCatalogProvider, RootPolicy, ScanOptions};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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

        fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, CtxError> {
            self.document_count
                .fetch_add(texts.len(), AtomicOrdering::SeqCst);
            self.inner.embed_documents(texts)
        }

        fn embed_query(&self, query: &str) -> Result<Vec<f32>, CtxError> {
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
