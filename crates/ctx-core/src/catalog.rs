//! Catalog providers.
//!
//! `MemoryCatalogProvider` is host-fed and works in wasm/browser/edge hosts. The
//! native-only `FsCatalogProvider` keeps the filesystem + ignore walker behind
//! the same `CatalogProvider` port.

#[cfg(not(target_arch = "wasm32"))]
use crate::port::FileSignature;
#[cfg(not(target_arch = "wasm32"))]
use crate::security::RootPolicy;
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
#[cfg(not(target_arch = "wasm32"))]
use ignore::WalkBuilder;
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, RwLock},
};
#[cfg(not(target_arch = "wasm32"))]
use std::{
    fmt, fs,
    sync::Mutex,
    time::{Duration, Instant},
};

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

mod memory_batch;

/// Options controlling native catalog scan cost.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub max_entries: usize,
    pub snapshot_cache_ttl: Duration,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            snapshot_cache_ttl: Duration::from_millis(1_000),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
type Clock = dyn Fn() -> Instant + Send + Sync;

/// Filesystem provider using an allow-listed root policy.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct FsCatalogProvider {
    policy: RootPolicy,
    options: ScanOptions,
    cache: Arc<ProviderCache>,
    clock: Arc<Clock>,
    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    semantic_index: Option<Arc<SemanticIndex>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl fmt::Debug for FsCatalogProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsCatalogProvider")
            .field("policy", &self.policy)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
struct ProviderCache {
    snapshot: RwLock<Option<CachedSnapshot>>,
    codemap: RwLock<HashMap<PathBuf, CachedCodeSymbols>>,
    selection: RwLock<Selection>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
struct CachedSnapshot {
    created_at: Instant,
    snapshot: Arc<CatalogSnapshot>,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
struct CachedCodeSymbols {
    signature: FileSignature,
    symbols: Arc<CodeSymbolsResult>,
}

#[cfg(not(target_arch = "wasm32"))]
impl FsCatalogProvider {
    #[must_use]
    pub fn new(policy: RootPolicy, options: ScanOptions) -> Self {
        Self::with_clock(policy, options, Instant::now)
    }

    #[must_use]
    pub fn with_clock<F>(policy: RootPolicy, options: ScanOptions, clock: F) -> Self
    where
        F: Fn() -> Instant + Send + Sync + 'static,
    {
        Self {
            policy,
            options,
            cache: Arc::new(ProviderCache::default()),
            clock: Arc::new(clock),
            #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
            semantic_index: None,
        }
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    #[must_use]
    pub fn with_semantic_index(
        policy: RootPolicy,
        options: ScanOptions,
        semantic_index: Option<Arc<SemanticIndex>>,
    ) -> Self {
        Self {
            policy,
            options,
            cache: Arc::new(ProviderCache::default()),
            clock: Arc::new(Instant::now),
            semantic_index,
        }
    }

    pub fn snapshot_arc(&self) -> Result<Arc<CatalogSnapshot>, CtxError> {
        self.snapshot_arc_cancellable(&CancelToken::never())
    }

    pub fn snapshot_arc_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<Arc<CatalogSnapshot>, CtxError> {
        cancel.check_cancelled()?;

        if self.policy.roots().is_empty() {
            return Err(CtxError::NoRoots);
        }

        let now = (self.clock)();
        if let Some(snapshot) = self.cached_snapshot(now) {
            return Ok(snapshot);
        }

        let mut guard = self.cache.snapshot.write().expect("snapshot cache lock");
        if let Some(cached) = guard.as_ref()
            && cache_entry_fresh(now, cached.created_at, self.options.snapshot_cache_ttl)
        {
            return Ok(Arc::clone(&cached.snapshot));
        }

        let snapshot = Arc::new(self.scan_snapshot_cancellable(cancel)?);
        *guard = Some(CachedSnapshot {
            created_at: now,
            snapshot: Arc::clone(&snapshot),
        });
        Ok(snapshot)
    }

    pub fn invalidate(&self) {
        *self.cache.snapshot.write().expect("snapshot cache lock") = None;
        self.cache
            .codemap
            .write()
            .expect("codemap cache lock")
            .clear();
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        if let Some(index) = &self.semantic_index {
            index.invalidate();
        }
    }

    #[must_use]
    pub fn roots(&self) -> &[RootRef] {
        self.policy.roots()
    }

    fn cached_snapshot(&self, now: Instant) -> Option<Arc<CatalogSnapshot>> {
        let guard = self.cache.snapshot.read().expect("snapshot cache lock");
        guard.as_ref().and_then(|cached| {
            cache_entry_fresh(now, cached.created_at, self.options.snapshot_cache_ttl)
                .then(|| Arc::clone(&cached.snapshot))
        })
    }

    fn scan_snapshot_cancellable(&self, cancel: &CancelToken) -> Result<CatalogSnapshot, CtxError> {
        let entries = Arc::new(Mutex::new(Vec::new()));
        let diagnostics = Arc::new(Mutex::new(Vec::new()));

        for root in self.policy.roots() {
            cancel.check_cancelled()?;
            scan_root(root, Arc::clone(&entries), Arc::clone(&diagnostics), cancel);
            cancel.check_cancelled()?;
        }

        finalize_snapshot(
            Arc::try_unwrap(entries)
                .expect("entries references dropped")
                .into_inner()
                .expect("entries lock"),
            Arc::try_unwrap(diagnostics)
                .expect("diagnostics references dropped")
                .into_inner()
                .expect("diagnostics lock"),
            self.policy.roots(),
            self.options.max_entries,
            cancel,
        )
    }

    fn file_signature(path: &Path) -> Result<FileSignature, CtxError> {
        let metadata = fs::metadata(path).map_err(|err| CtxError::io(path, err))?;
        Ok(FileSignature {
            modified: metadata.modified().ok(),
            size: metadata.len(),
        })
    }

    #[cfg(test)]
    fn codemap_cache_len(&self) -> usize {
        self.cache.codemap.read().expect("codemap cache lock").len()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_root(
    root: &RootRef,
    entries: Arc<Mutex<Vec<CatalogEntry>>>,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    cancel: &CancelToken,
) {
    let mut builder = WalkBuilder::new(&root.path);
    let filter_cancel = cancel.clone();
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |entry| include_walk_entry(entry, &filter_cancel));

    let context = ScanRootContext {
        root_path: root.path.clone(),
        root_id: root.id.clone(),
        entries,
        diagnostics,
        cancel: cancel.clone(),
    };
    builder
        .build_parallel()
        .run(|| scan_worker(context.clone()));
}

fn include_walk_entry(entry: &ignore::DirEntry, cancel: &CancelToken) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    !matches!(name.as_ref(), ".git" | "node_modules" | ".build" | "target")
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct ScanRootContext {
    root_path: PathBuf,
    root_id: String,
    entries: Arc<Mutex<Vec<CatalogEntry>>>,
    diagnostics: Arc<Mutex<Vec<Diagnostic>>>,
    cancel: CancelToken,
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_worker(
    context: ScanRootContext,
) -> Box<dyn FnMut(Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState + Send> {
    Box::new(move |dent| scan_entry(dent, &context))
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_entry(
    dent: Result<ignore::DirEntry, ignore::Error>,
    context: &ScanRootContext,
) -> ignore::WalkState {
    if context.cancel.is_cancelled() {
        return ignore::WalkState::Quit;
    }
    let dent = match dent {
        Ok(dent) => dent,
        Err(err) => {
            push_scan_diagnostic(&context.diagnostics, None, err.to_string());
            return ignore::WalkState::Continue;
        }
    };
    let path = dent.path();
    if !path.is_file() {
        return ignore::WalkState::Continue;
    }
    let metadata = match dent.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            push_scan_diagnostic(
                &context.diagnostics,
                Some(path.to_path_buf()),
                err.to_string(),
            );
            return ignore::WalkState::Continue;
        }
    };
    push_catalog_entry(path, metadata.len(), context);
    ignore::WalkState::Continue
}

#[cfg(not(target_arch = "wasm32"))]
fn push_scan_diagnostic(
    diagnostics: &Arc<Mutex<Vec<Diagnostic>>>,
    path: Option<PathBuf>,
    message: String,
) {
    diagnostics
        .lock()
        .expect("diagnostics lock")
        .push(Diagnostic { path, message });
}

fn push_catalog_entry(path: &Path, size: u64, context: &ScanRootContext) {
    let rel_path = path
        .strip_prefix(&context.root_path)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    context
        .entries
        .lock()
        .expect("entries lock")
        .push(CatalogEntry {
            root_id: context.root_id.clone(),
            rel_path,
            abs_path: path.to_path_buf(),
            size,
        });
}

#[cfg(not(target_arch = "wasm32"))]
fn finalize_snapshot(
    mut entries: Vec<CatalogEntry>,
    mut diagnostics: Vec<Diagnostic>,
    roots: &[RootRef],
    max_entries: usize,
    cancel: &CancelToken,
) -> Result<CatalogSnapshot, CtxError> {
    cancel.check_cancelled()?;
    if entries.len() > max_entries {
        return Err(CtxError::EntryLimitExceeded { limit: max_entries });
    }
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    diagnostics.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(CatalogSnapshot {
        generation: 1,
        roots: roots.to_vec(),
        entries,
        diagnostics,
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn cache_entry_fresh(now: Instant, created_at: Instant, ttl: Duration) -> bool {
    now.checked_duration_since(created_at)
        .is_some_and(|age| age <= ttl)
}

#[cfg(not(target_arch = "wasm32"))]
mod fs_atomic;

#[cfg(not(target_arch = "wasm32"))]
impl CatalogProvider for FsCatalogProvider {
    fn snapshot(&self) -> Result<CatalogSnapshot, CtxError> {
        self.snapshot_arc().map(|snapshot| (*snapshot).clone())
    }

    fn snapshot_arc(&self) -> Result<Arc<CatalogSnapshot>, CtxError> {
        FsCatalogProvider::snapshot_arc(self)
    }

    fn snapshot_arc_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<Arc<CatalogSnapshot>, CtxError> {
        FsCatalogProvider::snapshot_arc_cancellable(self, cancel)
    }

    fn invalidate(&self) {
        FsCatalogProvider::invalidate(self);
    }

    fn selection(&self) -> Selection {
        self.cache.selection.read().expect("selection lock").clone()
    }

    fn set_selection(&self, selection: Selection) {
        *self.cache.selection.write().expect("selection lock") = selection;
    }

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CtxError> {
        let allowed = self.policy.resolve_allowed(path)?;
        fs::read(&allowed).map_err(|err| CtxError::io(allowed, err))
    }

    fn file_signature(&self, path: &Path) -> Result<Option<FileSignature>, CtxError> {
        let allowed = self.policy.resolve_allowed(path)?;
        Ok(Some(Self::file_signature(&allowed)?))
    }

    fn validate_write_path(&self, path: &Path) -> Result<(), CtxError> {
        self.policy.resolve_for_write(path).map(|_| ())
    }

    fn write_text(&self, path: &Path, content: &str) -> Result<(), CtxError> {
        let target = self.policy.resolve_for_write(path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| CtxError::io(parent.to_path_buf(), err))?;
        }
        fs::write(&target, content).map_err(|err| CtxError::io(target.clone(), err))?;
        FsCatalogProvider::invalidate(self);
        Ok(())
    }

    fn delete_file(&self, path: &Path) -> Result<(), CtxError> {
        let target = self.policy.resolve_allowed(path)?;
        fs::remove_file(&target).map_err(|err| CtxError::io(target.clone(), err))?;
        FsCatalogProvider::invalidate(self);
        Ok(())
    }

    fn rename_file(&self, from: &Path, to: &Path) -> Result<(), CtxError> {
        let source = self.policy.resolve_allowed(from)?;
        let destination = self.policy.resolve_for_write(to)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|err| CtxError::io(parent.to_path_buf(), err))?;
        }
        fs::rename(&source, &destination).map_err(|err| CtxError::io(destination.clone(), err))?;
        FsCatalogProvider::invalidate(self);
        Ok(())
    }

    fn apply_file_batch(
        &self,
        changes: &[crate::edit::FileChange],
        atomic: bool,
    ) -> Result<(), CtxError> {
        if !atomic {
            for change in changes {
                fs_atomic::apply_change(self, change)?;
            }
            self.invalidate();
            return Ok(());
        }
        fs_atomic::apply_atomic_batch(self, changes)
    }

    fn code_symbols_for_path(
        &self,
        path: &Path,
        rel_path: &str,
    ) -> Result<CodeSymbolsResult, CtxError> {
        let allowed = self.policy.resolve_allowed(path)?;
        let signature = Self::file_signature(&allowed)?;
        if let Some(cached) = self
            .cache
            .codemap
            .read()
            .expect("codemap cache lock")
            .get(&allowed)
            .filter(|cached| cached.signature == signature)
        {
            return Ok((*cached.symbols).clone());
        }

        let bytes = fs::read(&allowed).map_err(|err| CtxError::io(&allowed, err))?;
        let source = String::from_utf8_lossy(&bytes);
        let parsed: CodeSymbolsResult =
            symbols_for_path(&source, rel_path).map(|maybe| maybe.map(Arc::new));
        self.cache
            .codemap
            .write()
            .expect("codemap cache lock")
            .insert(
                allowed,
                CachedCodeSymbols {
                    signature,
                    symbols: Arc::new(parsed.clone()),
                },
            );
        Ok(parsed)
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    fn semantic_index(&self) -> Option<Arc<SemanticIndex>> {
        self.semantic_index.clone()
    }

    fn display_path(&self, path: &Path) -> String {
        if let Ok(allowed) = self.policy.resolve_allowed(path)
            && let Some(root) = self
                .policy
                .roots()
                .iter()
                .find(|root| allowed.starts_with(&root.path))
        {
            let root_name = root.path.file_name().unwrap_or_default().to_string_lossy();
            let rel = allowed
                .strip_prefix(&root.path)
                .unwrap_or(&allowed)
                .to_string_lossy()
                .replace('\\', "/");
            if rel.is_empty() {
                return root_name.into_owned();
            }
            return format!("{root_name}/{rel}");
        }
        path.to_string_lossy().replace('\\', "/")
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;

    #[test]
    fn memory_provider_reads_host_fed_files_without_fs() {
        let provider =
            MemoryCatalogProvider::new(vec![HostFile::new("src/lib.rs", "pub fn alpha() {}\n")])
                .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        assert_eq!(snapshot.entries[0].rel_path, "src/lib.rs");
        assert_eq!(
            provider.read_bytes(Path::new("src/lib.rs")).expect("read"),
            b"pub fn alpha() {}\n"
        );
        assert_eq!(
            provider.display_path(Path::new("src/lib.rs")),
            "host/src/lib.rs"
        );
    }

    #[test]
    fn memory_provider_applies_atomic_batches() {
        let provider =
            MemoryCatalogProvider::new(vec![HostFile::new("a.txt", "alpha\n")]).expect("provider");
        provider
            .apply_file_batch(
                &[
                    crate::edit::FileChange::Update {
                        path: "a.txt".to_string(),
                        content: "ALPHA\n".to_string(),
                    },
                    crate::edit::FileChange::Create {
                        path: "b.txt".to_string(),
                        content: "beta\n".to_string(),
                    },
                ],
                true,
            )
            .expect("atomic batch");
        assert_eq!(provider.read_bytes(Path::new("a.txt")).unwrap(), b"ALPHA\n");
        assert_eq!(provider.read_bytes(Path::new("b.txt")).unwrap(), b"beta\n");
    }

    #[test]
    fn memory_provider_rejects_traversal() {
        let err = MemoryCatalogProvider::new(vec![HostFile::new("../secret", "nope")]).unwrap_err();
        assert!(matches!(err, CtxError::PathTraversal(_)));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::io::Write;

    fn paths(snapshot: &CatalogSnapshot) -> Vec<&str> {
        snapshot
            .entries
            .iter()
            .map(|entry| entry.rel_path.as_str())
            .collect()
    }

    #[test]
    fn scans_files_and_excludes_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("src")).expect("src");
        fs::write(dir.path().join("src/lib.rs"), "pub fn ok() {}\n").expect("write");
        fs::create_dir(dir.path().join("target")).expect("target");
        fs::write(dir.path().join("target/skip.txt"), "skip").expect("write skip");
        let mut file = fs::File::create(dir.path().join("README.md")).expect("readme");
        writeln!(file, "hello").expect("write readme");

        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        assert_eq!(paths(&snapshot), vec!["README.md", "src/lib.rs"]);
    }

    #[test]
    fn cache_reuses_snapshot_within_ttl() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "a").expect("a");
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock_now = Arc::clone(&now);
        let provider = FsCatalogProvider::with_clock(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
            move || *clock_now.lock().expect("clock"),
        );
        let first = provider.snapshot_arc().expect("first");
        fs::write(dir.path().join("b.txt"), "b").expect("b");
        let second = provider.snapshot_arc().expect("second");
        assert!(Arc::ptr_eq(&first, &second));
        *now.lock().expect("clock") += Duration::from_secs(2);
        let third = provider.snapshot_arc().expect("third");
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(paths(&third), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn invalidation_clears_snapshot_and_codemap_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lib.rs");
        fs::write(&path, "pub fn one() {}\n").expect("write one");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        provider
            .code_symbols_for_path(&path, "lib.rs")
            .expect("codemap")
            .expect("parse");
        assert_eq!(provider.codemap_cache_len(), 1);
        provider.invalidate();
        assert_eq!(provider.codemap_cache_len(), 0);
    }
}
