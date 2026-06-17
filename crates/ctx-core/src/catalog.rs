//! Catalog providers.
//!
//! `MemoryCatalogProvider` is host-fed and works in wasm/browser/edge hosts. The
//! native-only `FsCatalogProvider` keeps the filesystem + ignore walker behind
//! the same `CatalogProvider` port.

#[cfg(not(target_arch = "wasm32"))]
use crate::codemap::path_language_name;
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
    path::{Path, PathBuf},
    sync::{Arc, RwLock, Weak},
};
#[cfg(not(target_arch = "wasm32"))]
use std::{
    fmt, fs, mem,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        mpsc,
    },
    time::{Duration, Instant},
};

mod memory;
pub use memory::{HostFile, MemoryCatalogProvider};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Maximum entries returned in a snapshot; scans above this size are truncated with a diagnostic.
    pub max_entries: usize,
    pub snapshot_cache_ttl: Duration,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            snapshot_cache_ttl: Duration::from_millis(5_000),
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
    codemap_warming: Mutex<Option<CodemapWarmInProgress>>,
    generation: AtomicU64,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone)]
struct CodemapWarmInProgress {
    generation: u64,
    cancel: CancelToken,
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
        let generation = self.cache.generation.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        *guard = Some(CachedSnapshot {
            created_at: now,
            snapshot: Arc::clone(&snapshot),
        });
        drop(guard);
        self.start_codemap_warming(Arc::clone(&snapshot), generation);
        Ok(snapshot)
    }

    pub fn invalidate(&self) {
        *self.cache.snapshot.write().expect("snapshot cache lock") = None;
        self.cache.generation.fetch_add(1, AtomicOrdering::SeqCst);
        self.cache
            .codemap
            .write()
            .expect("codemap cache lock")
            .clear();
        if let Some(warming) = self
            .cache
            .codemap_warming
            .lock()
            .expect("codemap warming lock")
            .take()
        {
            warming.cancel.cancel();
        }
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
        let mut entries = Vec::new();
        let mut diagnostics = Vec::new();

        for root in self.policy.roots() {
            cancel.check_cancelled()?;
            let scan_output = scan_root(root, cancel);
            entries.extend(scan_output.entries);
            diagnostics.extend(scan_output.diagnostics);
            cancel.check_cancelled()?;
        }

        finalize_snapshot(
            entries,
            diagnostics,
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

    fn start_codemap_warming(&self, snapshot: Arc<CatalogSnapshot>, generation: u64) {
        if snapshot
            .entries
            .iter()
            .all(|entry| path_language_name(&entry.rel_path).is_none())
        {
            return;
        }
        let cancel = CancelToken::never();
        {
            let mut warming = self
                .cache
                .codemap_warming
                .lock()
                .expect("codemap warming lock");
            // Snapshot cache TTL/invalidation can rebuild snapshots frequently. One
            // best-effort warmer per provider generation is enough because cache
            // entries are signature-checked, and stale generations are forbidden
            // from inserting below. Replacing a generation cancels the older warmer
            // instead of letting duplicate parsers compete for the same cache.
            if warming
                .as_ref()
                .is_some_and(|current| current.generation == generation)
            {
                return;
            }
            if let Some(previous) = warming.take() {
                previous.cancel.cancel();
            }
            *warming = Some(CodemapWarmInProgress {
                generation,
                cancel: cancel.clone(),
            });
        }

        let cache = Arc::downgrade(&self.cache);
        let policy = self.policy.clone();
        std::thread::spawn(move || {
            Self::warm_codemap_for_snapshot(&cache, &policy, &snapshot, generation, &cancel);
            if let Some(cache) = cache.upgrade() {
                let mut warming = cache.codemap_warming.lock().expect("codemap warming lock");
                if warming
                    .as_ref()
                    .is_some_and(|current| current.generation == generation)
                {
                    *warming = None;
                }
            }
        });
    }

    fn warm_codemap_for_snapshot(
        cache: &Weak<ProviderCache>,
        policy: &RootPolicy,
        snapshot: &CatalogSnapshot,
        generation: u64,
        cancel: &CancelToken,
    ) {
        for entry in &snapshot.entries {
            let Some(cache) = cache.upgrade() else {
                return;
            };
            if cancel.check_cancelled().is_err()
                || cache.generation.load(AtomicOrdering::SeqCst) != generation
            {
                return;
            }
            if path_language_name(&entry.rel_path).is_none() {
                continue;
            }
            let Ok(allowed) = policy.resolve_allowed(&entry.abs_path) else {
                continue;
            };
            let _ =
                Self::code_symbols_for_allowed(&cache, &allowed, &entry.rel_path, Some(generation));
        }
    }

    fn code_symbols_for_allowed(
        cache: &ProviderCache,
        allowed: &Path,
        rel_path: &str,
        expected_generation: Option<u64>,
    ) -> Result<CodeSymbolsResult, CtxError> {
        let signature = Self::file_signature(allowed)?;
        if let Some(cached) = cache
            .codemap
            .read()
            .expect("codemap cache lock")
            .get(allowed)
            .filter(|cached| cached.signature == signature)
        {
            return Ok((*cached.symbols).clone());
        }

        let bytes = fs::read(allowed).map_err(|err| CtxError::io(allowed, err))?;
        let source = String::from_utf8_lossy(&bytes);
        let parsed: CodeSymbolsResult =
            symbols_for_path(&source, rel_path).map(|maybe| maybe.map(Arc::new));
        let generation_current = |expected_generation: Option<u64>| {
            expected_generation.is_none_or(|generation| {
                cache.generation.load(AtomicOrdering::SeqCst) == generation
            })
        };
        if generation_current(expected_generation) {
            let mut codemap = cache.codemap.write().expect("codemap cache lock");
            if generation_current(expected_generation) {
                codemap.insert(
                    allowed.to_path_buf(),
                    CachedCodeSymbols {
                        signature,
                        symbols: Arc::new(parsed.clone()),
                    },
                );
            }
        }
        Ok(parsed)
    }

    #[cfg(test)]
    fn codemap_cache_len(&self) -> usize {
        self.cache.codemap.read().expect("codemap cache lock").len()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_root(root: &RootRef, cancel: &CancelToken) -> ScanRootOutput {
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
        cancel: cancel.clone(),
    };
    let (sender, receiver) = mpsc::channel();
    builder
        .build_parallel()
        .run(|| scan_worker(context.clone(), sender.clone()));
    drop(sender);

    let mut output = ScanRootOutput::default();
    for worker_output in receiver {
        output.entries.extend(worker_output.entries);
        output.diagnostics.extend(worker_output.diagnostics);
    }
    output
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
    cancel: CancelToken,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct ScanRootOutput {
    entries: Vec<CatalogEntry>,
    diagnostics: Vec<Diagnostic>,
}

#[cfg(not(target_arch = "wasm32"))]
struct ScanWorkerState {
    context: ScanRootContext,
    entries: Vec<CatalogEntry>,
    diagnostics: Vec<Diagnostic>,
    sender: Option<mpsc::Sender<ScanRootOutput>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for ScanWorkerState {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(ScanRootOutput {
                entries: mem::take(&mut self.entries),
                diagnostics: mem::take(&mut self.diagnostics),
            });
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_worker(
    context: ScanRootContext,
    sender: mpsc::Sender<ScanRootOutput>,
) -> Box<dyn FnMut(Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState + Send> {
    let mut state = ScanWorkerState {
        context,
        entries: Vec::new(),
        diagnostics: Vec::new(),
        sender: Some(sender),
    };
    Box::new(move |dent| {
        let ScanWorkerState {
            context,
            entries,
            diagnostics,
            sender: _,
        } = &mut state;
        scan_entry(dent, context, entries, diagnostics)
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn scan_entry(
    dent: Result<ignore::DirEntry, ignore::Error>,
    context: &ScanRootContext,
    entries: &mut Vec<CatalogEntry>,
    diagnostics: &mut Vec<Diagnostic>,
) -> ignore::WalkState {
    if context.cancel.is_cancelled() {
        return ignore::WalkState::Quit;
    }
    let dent = match dent {
        Ok(dent) => dent,
        Err(err) => {
            push_scan_diagnostic(diagnostics, None, err.to_string());
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
            push_scan_diagnostic(diagnostics, Some(path.to_path_buf()), err.to_string());
            return ignore::WalkState::Continue;
        }
    };
    push_catalog_entry(entries, path, metadata.len(), context);
    ignore::WalkState::Continue
}

#[cfg(not(target_arch = "wasm32"))]
fn push_scan_diagnostic(diagnostics: &mut Vec<Diagnostic>, path: Option<PathBuf>, message: String) {
    diagnostics.push(Diagnostic { path, message });
}

fn push_catalog_entry(
    entries: &mut Vec<CatalogEntry>,
    path: &Path,
    size: u64,
    context: &ScanRootContext,
) {
    let rel_path = path
        .strip_prefix(&context.root_path)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    entries.push(CatalogEntry {
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
    entries.sort_by(|left, right| {
        left.rel_path
            .cmp(&right.rel_path)
            .then_with(|| left.root_id.cmp(&right.root_id))
            .then_with(|| left.abs_path.cmp(&right.abs_path))
    });
    if entries.len() > max_entries {
        let dropped = entries.len() - max_entries;
        entries.truncate(max_entries);
        diagnostics.push(Diagnostic {
            path: None,
            message: format!(
                "catalog scan truncated to {max_entries} entries; dropped {dropped} entries due to max_entries limit"
            ),
        });
    }
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
        Self::code_symbols_for_allowed(&self.cache, &allowed, rel_path, None)
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
    use std::{io::Write, sync::Mutex, thread, time::Duration as StdDuration};

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
    fn max_entries_truncates_with_diagnostic() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("b.txt"), "b").expect("b");
        fs::write(dir.path().join("a.txt"), "a").expect("a");
        fs::write(dir.path().join("c.txt"), "c").expect("c");

        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions {
                max_entries: 2,
                ..ScanOptions::default()
            },
        );
        let snapshot = provider.snapshot().expect("snapshot");

        assert_eq!(paths(&snapshot), vec!["a.txt", "b.txt"]);
        assert_eq!(snapshot.diagnostics.len(), 1);
        assert_eq!(snapshot.diagnostics[0].path, None);
        assert!(
            snapshot.diagnostics[0]
                .message
                .contains("catalog scan truncated to 2 entries; dropped 1 entries")
        );
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
        *now.lock().expect("clock") += Duration::from_secs(6);
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

    #[test]
    fn snapshot_starts_background_codemap_warming() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("lib.rs"), "pub fn warmed() {}\n").expect("write lib");
        fs::write(dir.path().join("notes.txt"), "plain text\n").expect("write notes");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );

        let snapshot = provider.snapshot().expect("snapshot");
        assert_eq!(paths(&snapshot), vec!["lib.rs", "notes.txt"]);

        for _ in 0..100 {
            if provider.codemap_cache_len() == 1 {
                return;
            }
            thread::sleep(StdDuration::from_millis(10));
        }
        assert_eq!(provider.codemap_cache_len(), 1);
    }

    #[test]
    fn stale_codemap_warmer_does_not_insert_after_invalidation() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("lib.rs"), "pub fn stale() {}\n").expect("write lib");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider
            .scan_snapshot_cancellable(&CancelToken::never())
            .expect("snapshot");
        let stale_generation = provider.cache.generation.load(AtomicOrdering::SeqCst);

        provider.invalidate();
        FsCatalogProvider::warm_codemap_for_snapshot(
            &Arc::downgrade(&provider.cache),
            &provider.policy,
            &snapshot,
            stale_generation,
            &CancelToken::never(),
        );

        assert_eq!(provider.codemap_cache_len(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn codemap_warmer_revalidates_root_policy_before_reading() {
        use std::os::unix::fs as unix_fs;

        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let inside_path = root.path().join("lib.rs");
        let outside_path = outside.path().join("lib.rs");
        fs::write(&inside_path, "pub fn inside() {}\n").expect("write inside");
        fs::write(&outside_path, "pub fn outside() {}\n").expect("write outside");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![root.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider
            .scan_snapshot_cancellable(&CancelToken::never())
            .expect("snapshot");
        let generation = provider.cache.generation.load(AtomicOrdering::SeqCst);

        fs::remove_file(&inside_path).expect("remove inside");
        unix_fs::symlink(&outside_path, &inside_path).expect("symlink outside");
        FsCatalogProvider::warm_codemap_for_snapshot(
            &Arc::downgrade(&provider.cache),
            &provider.policy,
            &snapshot,
            generation,
            &CancelToken::never(),
        );

        assert_eq!(provider.codemap_cache_len(), 0);
    }
}
