use super::*;

#[derive(Clone, Debug)]
pub(super) struct SnapshotFileState {
    pub(super) files: Vec<SnapshotFileRecord>,
    pub(super) fingerprint: String,
}

#[derive(Clone, Debug)]
pub(super) struct SnapshotFileRecord {
    pub(super) key: String,
    pub(super) entry: CatalogEntry,
    pub(super) signature: PersistedFileSignature,
}

impl SnapshotFileState {
    pub(super) fn from_snapshot<P: CatalogProvider + Sync>(
        provider: &P,
        snapshot: &CatalogSnapshot,
        scope: &SemanticIndexScope,
    ) -> Result<Self, NerveError> {
        let filter = scope.entry_filter()?;
        let mut files = Vec::with_capacity(snapshot.entries.len());
        let mut hasher = Sha256::new();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| filter.accepts(&entry.rel_path))
        {
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
pub(super) struct PersistedFileSignature {
    pub(super) modified_unix_nanos: Option<i128>,
    pub(super) size: u64,
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
pub(super) struct PersistedFileRecord {
    pub(super) file_key: String,
    pub(super) root_id: String,
    pub(super) rel_path: String,
    pub(super) signature: PersistedFileSignature,
    pub(super) chunk_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PersistedCurrent {
    pub(super) generation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PersistedManifest {
    pub(super) schema_version: u32,
    pub(super) chunker_version: u32,
    pub(super) workspace_key: String,
    pub(super) embedding_model_id: String,
    pub(super) embedding_dimension: usize,
    pub(super) roots: Vec<String>,
    pub(super) active_count: usize,
    pub(super) tombstone_count: usize,
    pub(super) files: Vec<PersistedFileRecord>,
    pub(super) chunks: Vec<PersistedChunkRecord>,
    pub(super) diagnostics: Vec<Diagnostic>,
    pub(super) embeddings: EmbeddingArtifact,
    pub(super) ann: AnnArtifact,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct PersistedChunkRecord {
    pub(super) chunk: SemanticChunk,
    pub(super) file_key: String,
    pub(super) embedding_row: usize,
    pub(super) active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct EmbeddingArtifact {
    pub(super) path: String,
    pub(super) rows: usize,
    pub(super) dimension: usize,
    pub(super) sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct AnnArtifact {
    pub(super) path: String,
    pub(super) rebuilt_from_embeddings: bool,
    pub(super) active_count: usize,
}

#[derive(Clone, Debug)]
pub(super) struct LoadedPersistedIndex {
    pub(super) files: BTreeMap<String, PersistedFileRecord>,
    pub(super) records: Vec<SemanticChunkRecord>,
}

pub(super) fn semantic_persistence_config(
    configured_dir: Option<&Path>,
    roots: &[RootRef],
    embedding_model_id: &str,
    embedding_dimension: usize,
    scope: &SemanticIndexScope,
) -> Result<Option<SemanticPersistenceConfig>, NerveError> {
    let cache_base_dir = configured_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(default_semantic_cache_dir);
    let canonical_roots = roots
        .iter()
        .map(|root| root.path.clone())
        .collect::<Vec<_>>();
    let workspace_key = workspace_key(
        &canonical_roots,
        embedding_model_id,
        embedding_dimension,
        &scope.cache_fingerprint(),
    );
    Ok(Some(SemanticPersistenceConfig {
        cache_base_dir,
        workspace_key,
        roots: canonical_roots,
        embedding_model_id: embedding_model_id.to_string(),
        embedding_dimension,
        generation_clock: GenerationClock::default(),
    }))
}

pub(super) fn workspace_key(
    roots: &[PathBuf],
    embedding_model_id: &str,
    embedding_dimension: usize,
    scope_fingerprint: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SCHEMA_VERSION.to_le_bytes());
    hasher.update(CHUNKER_VERSION.to_le_bytes());
    hasher.update(embedding_model_id.as_bytes());
    hasher.update(embedding_dimension.to_le_bytes());
    hasher.update(scope_fingerprint.as_bytes());
    for root in roots {
        hasher.update(root.to_string_lossy().as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

/// Machine-level cache root shared by the embedding-model cache and the
/// persistent index, so a model downloads once per machine (not per workspace).
pub(super) fn semantic_cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("nerve-workstation");
    }
    if let Ok(dir) = std::env::var("LOCALAPPDATA") {
        return PathBuf::from(dir).join("nerve-workstation");
    }
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        #[cfg(target_os = "macos")]
        {
            return home.join("Library/Caches/nerve-workstation");
        }
        #[cfg(not(target_os = "macos"))]
        {
            return home.join(".cache/nerve-workstation");
        }
    }
    std::env::temp_dir().join("nerve-workstation")
}

pub(super) fn default_semantic_cache_dir() -> PathBuf {
    semantic_cache_root().join("semantic")
}

pub(super) fn cache_workspace_dir(config: &SemanticPersistenceConfig) -> PathBuf {
    config.cache_base_dir.join(&config.workspace_key)
}

pub(super) fn current_path(config: &SemanticPersistenceConfig) -> PathBuf {
    cache_workspace_dir(config).join("current.json")
}

pub(super) fn generation_dir(config: &SemanticPersistenceConfig, generation: &str) -> PathBuf {
    cache_workspace_dir(config)
        .join("generations")
        .join(generation)
}

pub(super) fn load_persisted_index(
    config: &SemanticPersistenceConfig,
) -> Result<Option<LoadedPersistedIndex>, NerveError> {
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
    let bytes = fs::read(&embedding_path).map_err(|err| NerveError::io(&embedding_path, err))?;
    if sha256_hex(&bytes) != manifest.embeddings.sha256 {
        return Err(NerveError::Semantic(
            "semantic embedding artifact checksum mismatch".into(),
        ));
    }
    if embeddings.len() != manifest.chunks.len() {
        return Err(NerveError::Semantic(
            "semantic embedding/chunk row mismatch".into(),
        ));
    }
    let mut records = Vec::with_capacity(manifest.chunks.len());
    for chunk in manifest.chunks {
        let embedding = embeddings
            .get(chunk.embedding_row)
            .cloned()
            .ok_or_else(|| NerveError::Semantic("semantic embedding row out of range".into()))?;
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

pub(super) fn save_persisted_index(
    config: &SemanticPersistenceConfig,
    records: &[SemanticChunkRecord],
    files: &BTreeMap<String, PersistedFileRecord>,
    diagnostics: Vec<Diagnostic>,
    ann: &DenseAnn,
) -> Result<(), NerveError> {
    let generation = config.generation_clock.generation_id();
    let dir = generation_dir(config, &generation);
    fs::create_dir_all(&dir).map_err(|err| NerveError::io(&dir, err))?;
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
                NerveError::Semantic(format!("semantic ann metadata encode failed: {err}"))
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
        .map_err(|err| NerveError::Semantic(format!("semantic manifest encode failed: {err}")))?;
    write_synced(&dir.join("manifest.json"), &manifest_bytes)?;
    sync_dir(&dir)?;
    let current_bytes = serde_json::to_vec_pretty(&PersistedCurrent { generation })
        .map_err(|err| NerveError::Semantic(format!("semantic current encode failed: {err}")))?;
    write_atomic(
        &current_path(config),
        &current_bytes,
        &config.generation_clock,
    )
}

pub(super) fn validate_manifest(
    config: &SemanticPersistenceConfig,
    manifest: &PersistedManifest,
) -> Result<(), NerveError> {
    if manifest.schema_version != SCHEMA_VERSION
        || manifest.chunker_version != CHUNKER_VERSION
        || manifest.workspace_key != config.workspace_key
        || manifest.embedding_model_id != config.embedding_model_id
        || manifest.embedding_dimension != config.embedding_dimension
    {
        return Err(NerveError::Semantic(
            "semantic cache manifest is incompatible".into(),
        ));
    }
    Ok(())
}

pub(super) fn clean_workspace_cache(config: &SemanticPersistenceConfig) -> Result<(), NerveError> {
    let dir = cache_workspace_dir(config);
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|err| NerveError::io(&dir, err))?;
    }
    Ok(())
}

pub(super) fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, NerveError> {
    let bytes = fs::read(path).map_err(|err| NerveError::io(path, err))?;
    serde_json::from_slice(&bytes)
        .map_err(|err| NerveError::Semantic(format!("semantic cache JSON decode failed: {err}")))
}

pub(super) fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), NerveError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| NerveError::io(parent, err))?;
    }
    let mut file = File::create(path).map_err(|err| NerveError::io(path, err))?;
    file.write_all(bytes)
        .map_err(|err| NerveError::io(path, err))?;
    file.sync_all().map_err(|err| NerveError::io(path, err))
}

pub(super) fn write_atomic(
    path: &Path,
    bytes: &[u8],
    generation_clock: &GenerationClock,
) -> Result<(), NerveError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|err| NerveError::io(parent, err))?;
    // Draw a fresh stamp for the temp file (independent of any committed
    // generation id); the clock yields a unique value on each call.
    let tmp = parent.join(format!(".{}.tmp", generation_clock.generation_id()));
    write_synced(&tmp, bytes)?;
    rename_replace(&tmp, path)?;
    sync_dir(parent)
}

#[cfg(not(windows))]
pub(super) fn rename_replace(from: &Path, to: &Path) -> Result<(), NerveError> {
    fs::rename(from, to).map_err(|err| NerveError::io(to, err))
}

#[cfg(windows)]
pub(super) fn rename_replace(from: &Path, to: &Path) -> Result<(), NerveError> {
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
        return Err(NerveError::io(error_path, std::io::Error::last_os_error()));
    }
    Ok(())
}

pub(super) fn sync_dir(path: &Path) -> Result<(), NerveError> {
    match File::open(path).and_then(|file| file.sync_all()) {
        Ok(()) => Ok(()),
        Err(_) => Ok(()),
    }
}

pub(super) fn embeddings_to_bytes(records: &[SemanticChunkRecord]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for record in records {
        for value in &record.embedding {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes
}

pub(super) fn read_embeddings(
    path: &Path,
    rows: usize,
    dimension: usize,
) -> Result<Vec<Vec<f32>>, NerveError> {
    let expected = rows
        .checked_mul(dimension)
        .and_then(|values| values.checked_mul(std::mem::size_of::<f32>()))
        .ok_or_else(|| NerveError::Semantic("semantic embedding artifact size overflow".into()))?;
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|err| NerveError::io(path, err))?;
    if bytes.len() != expected {
        return Err(NerveError::Semantic(format!(
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

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn system_time_to_unix_nanos(time: SystemTime) -> Option<i128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i128::try_from(duration.as_nanos()).ok())
}

pub(super) fn file_key(root_id: &str, rel_path: &str) -> String {
    format!("{root_id}\0{rel_path}")
}

pub(super) fn tombstone_chunks(
    records: &mut [SemanticChunkRecord],
    file_key: &str,
    chunk_ids: &[String],
) {
    let chunk_ids: HashSet<&str> = chunk_ids.iter().map(String::as_str).collect();
    for record in records {
        if record.file_key == file_key && chunk_ids.contains(record.chunk.id.as_str()) {
            record.active = false;
        }
    }
}

pub(super) fn should_compact(
    total_records: usize,
    tombstones: usize,
    config: &SemanticIndexConfig,
) -> bool {
    tombstones > 0
        && (tombstones >= config.compaction_tombstone_count
            || (tombstones as f64 / total_records.max(1) as f64)
                >= config.compaction_tombstone_ratio)
}

pub(super) fn embedding_model_id(model: Option<&str>) -> String {
    match model.unwrap_or("jina-embeddings-v2-base-code") {
        "jinaai/jina-embeddings-v2-base-code" => "jina-embeddings-v2-base-code".to_string(),
        "BAAI/bge-small-en-v1.5" => "bge-small-en-v1.5".to_string(),
        other => other.to_string(),
    }
}
