#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
use crate::semantic::SemanticIndex;
use crate::{
    cancel::CancelToken,
    codemap::symbols_for_path,
    models::*,
    port::{CatalogProvider, CodeSymbolsResult},
    selection::Selection,
    snapshot::CatalogSnapshot,
};
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
};

#[path = "memory_batch.rs"]
mod memory_batch;

/// One host-provided file for `MemoryCatalogProvider`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFile {
    pub path: PathBuf,
    pub content: Vec<u8>,
}

impl HostFile {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            content: content.into(),
        }
    }
}

/// Host-fed in-memory catalog provider.
///
/// This provider never scans the filesystem and never consults ignore rules. It
/// is intended for browser/edge wasm hosts that already possess file paths and
/// contents and want to feed them through the same engine port used by native
/// filesystem hosts.
#[derive(Clone, Debug)]
pub struct MemoryCatalogProvider {
    root_id: String,
    root_path: PathBuf,
    state: Arc<MemoryProviderState>,
}

#[derive(Debug)]
struct MemoryProviderState {
    snapshot: RwLock<Arc<CatalogSnapshot>>,
    files: RwLock<HashMap<PathBuf, Arc<Vec<u8>>>>,
    codemap: RwLock<HashMap<PathBuf, Arc<CodeSymbolsResult>>>,
    selection: RwLock<Selection>,
    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    semantic_index: RwLock<Option<Arc<SemanticIndex>>>,
}

impl Default for MemoryProviderState {
    fn default() -> Self {
        Self {
            snapshot: RwLock::new(Arc::new(CatalogSnapshot {
                generation: 0,
                roots: Vec::new(),
                entries: Vec::new(),
                diagnostics: Vec::new(),
            })),
            files: RwLock::new(HashMap::new()),
            codemap: RwLock::new(HashMap::new()),
            selection: RwLock::new(Selection::default()),
            #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
            semantic_index: RwLock::new(None),
        }
    }
}

impl Default for MemoryCatalogProvider {
    fn default() -> Self {
        Self::empty()
    }
}

impl MemoryCatalogProvider {
    const DEFAULT_ROOT_ID: &'static str = "memory-root";
    const DEFAULT_ROOT_NAME: &'static str = "host";

    #[must_use]
    pub fn empty() -> Self {
        let provider = Self {
            root_id: Self::DEFAULT_ROOT_ID.to_string(),
            root_path: PathBuf::from(Self::DEFAULT_ROOT_NAME),
            state: Arc::new(MemoryProviderState::default()),
        };
        provider
            .replace_files(Vec::new())
            .expect("empty host files");
        provider
    }

    pub fn new(files: Vec<HostFile>) -> Result<Self, CtxError> {
        let provider = Self::empty();
        provider.replace_files(files)?;
        Ok(provider)
    }

    pub fn from_pairs<I, P, C>(files: I) -> Result<Self, CtxError>
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<PathBuf>,
        C: Into<Vec<u8>>,
    {
        Self::new(
            files
                .into_iter()
                .map(|(path, content)| HostFile::new(path, content))
                .collect(),
        )
    }

    pub fn replace_files(&self, files: Vec<HostFile>) -> Result<(), CtxError> {
        let mut entries = Vec::with_capacity(files.len());
        let mut map = HashMap::with_capacity(files.len());

        for file in files {
            let normalized = normalize_host_path(&file.path)?;
            let rel_path = path_to_slash_string(&normalized);
            let content = Arc::new(file.content);
            entries.push(CatalogEntry {
                root_id: self.root_id.clone(),
                rel_path,
                abs_path: normalized.clone(),
                size: content.len() as u64,
            });
            map.insert(normalized, content);
        }

        entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        let snapshot = CatalogSnapshot {
            generation: self
                .state
                .snapshot
                .read()
                .expect("memory snapshot lock")
                .generation
                .saturating_add(1),
            roots: vec![RootRef {
                id: self.root_id.clone(),
                path: self.root_path.clone(),
            }],
            entries,
            diagnostics: Vec::new(),
        };

        *self.state.files.write().expect("memory files lock") = map;
        *self.state.codemap.write().expect("memory codemap lock") = HashMap::new();
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        if let Some(index) = self
            .state
            .semantic_index
            .read()
            .expect("memory semantic index lock")
            .as_ref()
        {
            index.invalidate();
        }
        *self.state.snapshot.write().expect("memory snapshot lock") = Arc::new(snapshot);
        Ok(())
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    pub fn set_semantic_index(&self, index: Option<Arc<SemanticIndex>>) {
        *self
            .state
            .semantic_index
            .write()
            .expect("memory semantic index lock") = index;
    }

    #[must_use]
    pub fn root_id(&self) -> &str {
        &self.root_id
    }
}

impl CatalogProvider for MemoryCatalogProvider {
    fn snapshot(&self) -> Result<CatalogSnapshot, CtxError> {
        Ok((**self.state.snapshot.read().expect("memory snapshot lock")).clone())
    }

    fn snapshot_arc(&self) -> Result<Arc<CatalogSnapshot>, CtxError> {
        Ok(Arc::clone(
            &self.state.snapshot.read().expect("memory snapshot lock"),
        ))
    }

    fn snapshot_arc_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<Arc<CatalogSnapshot>, CtxError> {
        cancel.check_cancelled()?;
        let snapshot = self.snapshot_arc()?;
        cancel.check_cancelled()?;
        Ok(snapshot)
    }

    fn invalidate(&self) {
        self.state
            .codemap
            .write()
            .expect("memory codemap lock")
            .clear();
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        if let Some(index) = self
            .state
            .semantic_index
            .read()
            .expect("memory semantic index lock")
            .as_ref()
        {
            index.invalidate();
        }
    }

    fn selection(&self) -> Selection {
        self.state
            .selection
            .read()
            .expect("memory selection lock")
            .clone()
    }

    fn set_selection(&self, selection: Selection) {
        *self.state.selection.write().expect("memory selection lock") = selection;
    }

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CtxError> {
        let normalized = normalize_host_path(path)?;
        self.state
            .files
            .read()
            .expect("memory files lock")
            .get(&normalized)
            .map(|bytes| bytes.as_ref().clone())
            .ok_or_else(|| CtxError::OutsideRoots(path.to_path_buf()))
    }

    fn validate_write_path(&self, path: &Path) -> Result<(), CtxError> {
        normalize_host_path(path).map(|_| ())
    }

    fn write_text(&self, path: &Path, content: &str) -> Result<(), CtxError> {
        self.apply_file_batch(
            &[crate::edit::FileChange::Update {
                path: path_to_slash_string(path),
                content: content.to_string(),
            }],
            false,
        )
    }

    fn delete_file(&self, path: &Path) -> Result<(), CtxError> {
        self.apply_file_batch(
            &[crate::edit::FileChange::Delete {
                path: path_to_slash_string(path),
            }],
            false,
        )
    }

    fn rename_file(&self, from: &Path, to: &Path) -> Result<(), CtxError> {
        let content = String::from_utf8_lossy(&self.read_bytes(from)?).into_owned();
        self.apply_file_batch(
            &[crate::edit::FileChange::Rename {
                from: path_to_slash_string(from),
                to: path_to_slash_string(to),
                content,
            }],
            false,
        )
    }

    fn apply_file_batch(
        &self,
        changes: &[crate::edit::FileChange],
        _atomic: bool,
    ) -> Result<(), CtxError> {
        let mut guard = self.state.files.write().expect("memory files lock");
        let mut next = guard.clone();
        for change in changes {
            memory_batch::apply_change(&mut next, change)?;
        }
        *guard = next;
        let snapshot_files = guard.clone();
        drop(guard);
        memory_batch::refresh_snapshot_from_map(self, &snapshot_files);
        Ok(())
    }

    fn code_symbols_for_path(
        &self,
        path: &Path,
        rel_path: &str,
    ) -> Result<CodeSymbolsResult, CtxError> {
        let normalized = normalize_host_path(path)?;
        if let Some(cached) = self
            .state
            .codemap
            .read()
            .expect("memory codemap lock")
            .get(&normalized)
        {
            return Ok((**cached).clone());
        }

        let bytes = self.read_bytes(&normalized)?;
        let source = String::from_utf8_lossy(&bytes);
        let parsed: CodeSymbolsResult =
            symbols_for_path(&source, rel_path).map(|maybe| maybe.map(Arc::new));
        self.state
            .codemap
            .write()
            .expect("memory codemap lock")
            .insert(normalized, Arc::new(parsed.clone()));
        Ok(parsed)
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    fn semantic_index(&self) -> Option<Arc<SemanticIndex>> {
        self.state
            .semantic_index
            .read()
            .expect("memory semantic index lock")
            .clone()
    }

    fn display_path(&self, path: &Path) -> String {
        normalize_host_path(path).map_or_else(
            |_| path.to_string_lossy().replace('\\', "/"),
            |normalized| {
                format!(
                    "{}/{}",
                    self.root_path.display(),
                    path_to_slash_string(&normalized)
                )
            },
        )
    }
}

fn normalize_host_path(path: &Path) -> Result<PathBuf, CtxError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                if normalized.as_os_str().is_empty()
                    && part == MemoryCatalogProvider::DEFAULT_ROOT_NAME
                {
                    continue;
                }
                normalized.push(part);
            }
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                return Err(CtxError::PathTraversal(path.display().to_string()));
            }
            Component::Prefix(_) => {}
        }
    }
    if normalized.as_os_str().is_empty() {
        Err(CtxError::OutsideRoots(path.to_path_buf()))
    } else {
        Ok(normalized)
    }
}

fn path_to_slash_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}
