use super::*;

mod search;

#[derive(Debug, Clone)]
pub struct SemanticWarmResponse {
    pub files_in_scope: usize,
    pub chunks: usize,
    pub cache_dir: Option<PathBuf>,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct SemanticIndex {
    config: SemanticIndexConfig,
    embedding: Arc<dyn EmbeddingBackend>,
    reranker: Option<Arc<dyn RerankerBackend>>,
    built: RwLock<Option<Arc<BuiltSemanticIndex>>>,
    build_lock: Mutex<()>,
    building: Mutex<Option<BuildInProgress>>,
    last_build_error: Mutex<Option<String>>,
    generation: AtomicU64,
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
            building: Mutex::new(None),
            last_build_error: Mutex::new(None),
            generation: AtomicU64::new(0),
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
        self.generation.fetch_add(1, AtomicOrdering::SeqCst);
        *self.built.write().expect("semantic index lock") = None;
        *self.building.lock().expect("semantic building lock") = None;
        *self
            .last_build_error
            .lock()
            .expect("semantic build error lock") = None;
    }

    pub fn warm<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        cancel: &CancelToken,
    ) -> Result<SemanticWarmResponse, CtxError> {
        let built = self.ensure_built(provider, snapshot, cancel)?;
        Ok(SemanticWarmResponse {
            files_in_scope: self.scoped_entries(snapshot)?.len(),
            chunks: built.chunks.len(),
            cache_dir: self.cache_dir(),
            diagnostics: built.diagnostics.clone(),
        })
    }

    #[must_use]
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.config.persistence.as_ref().map(cache_workspace_dir)
    }

    pub fn purge_cache(&self) -> Result<Option<PathBuf>, CtxError> {
        let Some(persistence) = &self.config.persistence else {
            self.invalidate();
            return Ok(None);
        };
        let dir = cache_workspace_dir(persistence);
        clean_workspace_cache(persistence)?;
        self.invalidate();
        Ok(Some(dir))
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
        self.search_built(
            &built,
            snapshot.generation,
            snapshot.entries.len(),
            request,
            cancel,
        )
    }

    pub fn search_background<P>(
        self: &Arc<Self>,
        provider: P,
        snapshot: Arc<CatalogSnapshot>,
        request: &SemanticSearchRequest,
        cancel: &CancelToken,
    ) -> Result<SemanticSearchResponse, CtxError>
    where
        P: CatalogProvider + Clone + Send + Sync + 'static,
    {
        cancel.check_cancelled()?;
        if self.config.persistence.is_some() {
            let state = SnapshotFileState::from_snapshot(&provider, &snapshot, &self.config.scope)?;
            if let Some(cached) = self.built.read().expect("semantic index lock").as_ref()
                && cached.manifest_fingerprint == state.fingerprint
            {
                return self.search_built(
                    cached,
                    snapshot.generation,
                    snapshot.entries.len(),
                    request,
                    cancel,
                );
            }
            let chunk_build = self.build_scoped_chunks(&provider, &snapshot, cancel)?;
            self.start_background_build(
                provider,
                Arc::clone(&snapshot),
                BackgroundBuildInput::Persistent(state),
            );
            return self.search_fallback(chunk_build, snapshot.entries.len(), request);
        }

        let chunk_build = self.build_scoped_chunks(&provider, &snapshot, cancel)?;
        if let Some(cached) = self.built.read().expect("semantic index lock").as_ref()
            && cached.manifest_fingerprint == chunk_build.manifest.fingerprint
        {
            return self.search_built(
                cached,
                snapshot.generation,
                snapshot.entries.len(),
                request,
                cancel,
            );
        }
        self.start_background_build(
            provider,
            Arc::clone(&snapshot),
            BackgroundBuildInput::Chunks(chunk_build.clone()),
        );
        self.search_fallback(chunk_build, snapshot.entries.len(), request)
    }

    pub fn search_if_ready<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        request: &SemanticSearchRequest,
        cancel: &CancelToken,
    ) -> Result<Option<SemanticSearchResponse>, CtxError> {
        cancel.check_cancelled()?;
        let Some(cached) = self
            .built
            .read()
            .expect("semantic index lock")
            .as_ref()
            .cloned()
        else {
            return Ok(None);
        };
        let is_current = if self.config.persistence.is_some() {
            let fingerprint =
                SnapshotFileState::from_snapshot(provider, snapshot, &self.config.scope)?
                    .fingerprint;
            cached.manifest_fingerprint == fingerprint
        } else {
            cached.snapshot_generation == Some(snapshot.generation)
        };
        if is_current {
            return self
                .search_built(
                    &cached,
                    snapshot.generation,
                    snapshot.entries.len(),
                    request,
                    cancel,
                )
                .map(Some);
        }
        Ok(None)
    }

    fn search_built(
        &self,
        built: &BuiltSemanticIndex,
        generation: u64,
        scanned_files: usize,
        request: &SemanticSearchRequest,
        cancel: &CancelToken,
    ) -> Result<SemanticSearchResponse, CtxError> {
        search::search_built(self, built, generation, scanned_files, request, cancel)
    }

    fn search_fallback(
        &self,
        chunk_build: ChunkBuild,
        scanned_files: usize,
        request: &SemanticSearchRequest,
    ) -> Result<SemanticSearchResponse, CtxError> {
        search::search_fallback(self, chunk_build, scanned_files, request)
    }

    fn start_background_build<P>(
        self: &Arc<Self>,
        provider: P,
        snapshot: Arc<CatalogSnapshot>,
        input: BackgroundBuildInput,
    ) where
        P: CatalogProvider + Clone + Send + Sync + 'static,
    {
        let fingerprint = input.fingerprint().to_string();
        let generation = self.generation.load(AtomicOrdering::SeqCst);
        {
            let mut building = self.building.lock().expect("semantic building lock");
            if building.as_ref().is_some_and(|current| {
                current.fingerprint == fingerprint && current.generation == generation
            }) {
                return;
            }
            *building = Some(BuildInProgress {
                fingerprint: fingerprint.clone(),
                generation,
            });
        }

        let index = Arc::clone(self);
        std::thread::spawn(move || {
            let cancel = CancelToken::never();
            let result = match input {
                BackgroundBuildInput::Persistent(state) => index.build_with_persistence(
                    &provider,
                    &snapshot,
                    &state,
                    &cancel,
                    Some(generation),
                ),
                BackgroundBuildInput::Chunks(chunk_build) => {
                    index.build_from_chunks(chunk_build, &cancel)
                }
            };
            let mut building = index.building.lock().expect("semantic building lock");
            let still_current = index.generation.load(AtomicOrdering::SeqCst) == generation
                && building.as_ref().is_some_and(|current| {
                    current.fingerprint == fingerprint && current.generation == generation
                });
            if still_current {
                match result {
                    Ok(built) => {
                        *index.built.write().expect("semantic index lock") = Some(Arc::new(built));
                        *index
                            .last_build_error
                            .lock()
                            .expect("semantic build error lock") = None;
                    }
                    Err(err) => {
                        *index
                            .last_build_error
                            .lock()
                            .expect("semantic build error lock") = Some(err.to_string());
                    }
                }
                *building = None;
            }
        });
    }

    fn scoped_entries<'a>(
        &self,
        snapshot: &'a CatalogSnapshot,
    ) -> Result<Vec<&'a CatalogEntry>, CtxError> {
        let filter = self.config.scope.entry_filter()?;
        Ok(snapshot
            .entries
            .iter()
            .filter(|entry| filter.accepts(&entry.rel_path))
            .collect())
    }

    fn build_scoped_chunks<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        cancel: &CancelToken,
    ) -> Result<ChunkBuild, CtxError> {
        let entries = self.scoped_entries(snapshot)?;
        build_chunks_for_entries(provider, &entries, snapshot.generation, cancel)
    }

    fn ensure_built<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        cancel: &CancelToken,
    ) -> Result<Arc<BuiltSemanticIndex>, CtxError> {
        if self.config.persistence.is_some() {
            let state = SnapshotFileState::from_snapshot(provider, snapshot, &self.config.scope)?;
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
            let built =
                Arc::new(self.build_with_persistence(provider, snapshot, &state, cancel, None)?);
            *self.built.write().expect("semantic index lock") = Some(Arc::clone(&built));
            return Ok(built);
        }

        let chunk_build = self.build_scoped_chunks(provider, snapshot, cancel)?;
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
            Some(chunk_build.generation),
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
        write_generation: Option<u64>,
    ) -> Result<BuiltSemanticIndex, CtxError> {
        let persistence = self
            .config
            .persistence
            .as_ref()
            .expect("persistence config checked");
        let loaded = load_or_clean_persisted_index(persistence);
        let mut rebuild = reconcile_persisted_files(loaded.as_ref(), state);
        self.rebuild_changed_entries(provider, snapshot, state, &mut rebuild, cancel)?;
        compact_records_if_needed(&mut rebuild.records, &self.config);

        let built = self.built_from_records(
            state.fingerprint.clone(),
            &rebuild.records,
            rebuild.diagnostics.clone(),
        )?;
        if write_generation
            .is_some_and(|expected| self.generation.load(AtomicOrdering::SeqCst) != expected)
        {
            return Ok(built);
        }
        save_or_attach_cache_diagnostic(persistence, rebuild, built)
    }

    fn rebuild_changed_entries<P: CatalogProvider + Sync>(
        &self,
        provider: &P,
        snapshot: &CatalogSnapshot,
        state: &SnapshotFileState,
        rebuild: &mut PersistenceRebuild,
        cancel: &CancelToken,
    ) -> Result<(), CtxError> {
        for entry in rebuild.changed_entries.drain(..) {
            cancel.check_cancelled()?;
            let build = build_chunks_for_entries(provider, &[&entry], snapshot.generation, cancel)?;
            rebuild.diagnostics.extend(build.diagnostics);
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
            push_rebuilt_chunks(&mut rebuild.records, build.chunks, embeddings, &file_key);
            rebuild.files.insert(
                file_key.clone(),
                rebuilt_file_record(entry, state, file_key, chunk_ids),
            );
        }
        Ok(())
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
            None,
            chunks,
            vectors,
            diagnostics,
            self.embedding.dimension(),
        )
    }

    fn built_from_active(
        fingerprint: String,
        snapshot_generation: Option<u64>,
        chunks: Vec<SemanticChunk>,
        vectors: Vec<Vec<f32>>,
        diagnostics: Vec<Diagnostic>,
        dimension: usize,
    ) -> Result<BuiltSemanticIndex, CtxError> {
        let ann = DenseAnn::new(vectors, dimension)?;
        let bm25 = ChunkBm25::new(&chunks);
        Ok(BuiltSemanticIndex {
            manifest_fingerprint: fingerprint,
            snapshot_generation,
            chunks,
            diagnostics,
            ann,
            bm25,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct SemanticChunkRecord {
    pub(super) chunk: SemanticChunk,
    pub(super) embedding: Vec<f32>,
    pub(super) active: bool,
    pub(super) file_key: String,
}

pub(super) struct BuiltSemanticIndex {
    pub(super) manifest_fingerprint: String,
    pub(super) snapshot_generation: Option<u64>,
    pub(super) chunks: Vec<SemanticChunk>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) ann: DenseAnn,
    pub(super) bm25: ChunkBm25,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BuildInProgress {
    pub(super) fingerprint: String,
    pub(super) generation: u64,
}

pub(super) enum BackgroundBuildInput {
    Persistent(SnapshotFileState),
    Chunks(ChunkBuild),
}

impl BackgroundBuildInput {
    fn fingerprint(&self) -> &str {
        match self {
            Self::Persistent(state) => &state.fingerprint,
            Self::Chunks(build) => &build.manifest.fingerprint,
        }
    }
}
