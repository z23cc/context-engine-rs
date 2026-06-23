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
    path::{Path, PathBuf},
    sync::{Arc, RwLock, Weak},
    time::Duration,
};
#[cfg(not(target_arch = "wasm32"))]
use std::{
    fmt, fs,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::Instant,
};

#[cfg(not(target_arch = "wasm32"))]
mod fs_scan;
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
        }
    }

    pub fn snapshot_arc(&self) -> Result<Arc<CatalogSnapshot>, NerveError> {
        self.snapshot_arc_cancellable(&CancelToken::never())
    }

    pub fn snapshot_arc_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<Arc<CatalogSnapshot>, NerveError> {
        cancel.check_cancelled()?;

        if self.policy.roots().is_empty() {
            return Err(NerveError::NoRoots);
        }

        let now = (self.clock)();
        if let Some(snapshot) = self.cached_snapshot(now) {
            return Ok(snapshot);
        }

        let mut guard = crate::sync::write_recover(&self.cache.snapshot);
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
        *crate::sync::write_recover(&self.cache.snapshot) = None;
        self.cache.generation.fetch_add(1, AtomicOrdering::SeqCst);
        // The codemap parse cache is deliberately NOT cleared: it is keyed by the
        // per-file (mtime,size) signature and re-validated on every read, so a
        // changed file misses and re-parses while UNCHANGED files are reused. This
        // turns the post-edit / post-TTL re-query loop into incremental parsing
        // (only changed files re-parse) instead of a full re-parse — the dominant
        // cold cost the engine_hot_paths bench measured. Correctness is unchanged:
        // signature validation guards stale reads, and the generation guard (in
        // `code_symbols_for_allowed`) blocks a stale background warmer from
        // inserting. The cache stays bounded by the live path set (a changed file
        // overwrites its own key).
        if let Some(warming) = crate::sync::lock_recover(&self.cache.codemap_warming).take() {
            warming.cancel.cancel();
        }
    }

    #[must_use]
    pub fn roots(&self) -> &[RootRef] {
        self.policy.roots()
    }

    fn cached_snapshot(&self, now: Instant) -> Option<Arc<CatalogSnapshot>> {
        let guard = crate::sync::read_recover(&self.cache.snapshot);
        guard.as_ref().and_then(|cached| {
            cache_entry_fresh(now, cached.created_at, self.options.snapshot_cache_ttl)
                .then(|| Arc::clone(&cached.snapshot))
        })
    }

    fn scan_snapshot_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<CatalogSnapshot, NerveError> {
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

    fn file_signature(path: &Path) -> Result<FileSignature, NerveError> {
        let metadata = fs::metadata(path).map_err(|err| NerveError::io(path, err))?;
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
            let mut warming = crate::sync::lock_recover(&self.cache.codemap_warming);
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
                let mut warming = crate::sync::lock_recover(&cache.codemap_warming);
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
    ) -> Result<CodeSymbolsResult, NerveError> {
        let signature = Self::file_signature(allowed)?;
        if let Some(cached) = crate::sync::read_recover(&cache.codemap)
            .get(allowed)
            .filter(|cached| cached.signature == signature)
        {
            return Ok((*cached.symbols).clone());
        }

        let bytes = fs::read(allowed).map_err(|err| NerveError::io(allowed, err))?;
        let source = String::from_utf8_lossy(&bytes);
        let parsed: CodeSymbolsResult =
            symbols_for_path(&source, rel_path).map(|maybe| maybe.map(Arc::new));
        let generation_current = |expected_generation: Option<u64>| {
            expected_generation.is_none_or(|generation| {
                cache.generation.load(AtomicOrdering::SeqCst) == generation
            })
        };
        if generation_current(expected_generation) {
            let mut codemap = crate::sync::write_recover(&cache.codemap);
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
fn cache_entry_fresh(now: Instant, created_at: Instant, ttl: Duration) -> bool {
    now.checked_duration_since(created_at)
        .is_some_and(|age| age <= ttl)
}

#[cfg(not(target_arch = "wasm32"))]
use fs_scan::{finalize_snapshot, scan_root};

#[cfg(not(target_arch = "wasm32"))]
mod fs_atomic;

#[cfg(not(target_arch = "wasm32"))]
impl CatalogProvider for FsCatalogProvider {
    fn snapshot(&self) -> Result<CatalogSnapshot, NerveError> {
        self.snapshot_arc().map(|snapshot| (*snapshot).clone())
    }

    fn snapshot_arc(&self) -> Result<Arc<CatalogSnapshot>, NerveError> {
        FsCatalogProvider::snapshot_arc(self)
    }

    fn snapshot_arc_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<Arc<CatalogSnapshot>, NerveError> {
        FsCatalogProvider::snapshot_arc_cancellable(self, cancel)
    }

    fn invalidate(&self) {
        FsCatalogProvider::invalidate(self);
    }

    fn selection(&self) -> Selection {
        crate::sync::read_recover(&self.cache.selection).clone()
    }

    fn set_selection(&self, selection: Selection) {
        *crate::sync::write_recover(&self.cache.selection) = selection;
    }

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, NerveError> {
        let allowed = self.policy.resolve_allowed(path)?;
        fs::read(&allowed).map_err(|err| NerveError::io(allowed, err))
    }

    fn file_signature(&self, path: &Path) -> Result<Option<FileSignature>, NerveError> {
        let allowed = self.policy.resolve_allowed(path)?;
        Ok(Some(Self::file_signature(&allowed)?))
    }

    fn validate_write_path(&self, path: &Path) -> Result<(), NerveError> {
        self.policy.resolve_for_write(path).map(|_| ())
    }

    fn write_text(&self, path: &Path, content: &str) -> Result<(), NerveError> {
        let target = self.policy.resolve_for_write(path)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| NerveError::io(parent.to_path_buf(), err))?;
        }
        fs::write(&target, content).map_err(|err| NerveError::io(target.clone(), err))?;
        FsCatalogProvider::invalidate(self);
        Ok(())
    }

    fn delete_file(&self, path: &Path) -> Result<(), NerveError> {
        let target = self.policy.resolve_allowed(path)?;
        fs::remove_file(&target).map_err(|err| NerveError::io(target.clone(), err))?;
        FsCatalogProvider::invalidate(self);
        Ok(())
    }

    fn rename_file(&self, from: &Path, to: &Path) -> Result<(), NerveError> {
        let source = self.policy.resolve_allowed(from)?;
        let destination = self.policy.resolve_for_write(to)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|err| NerveError::io(parent.to_path_buf(), err))?;
        }
        fs::rename(&source, &destination)
            .map_err(|err| NerveError::io(destination.clone(), err))?;
        FsCatalogProvider::invalidate(self);
        Ok(())
    }

    fn apply_file_batch(
        &self,
        changes: &[crate::edit::FileChange],
        atomic: bool,
    ) -> Result<(), NerveError> {
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
    ) -> Result<CodeSymbolsResult, NerveError> {
        let allowed = self.policy.resolve_allowed(path)?;
        Self::code_symbols_for_allowed(&self.cache, &allowed, rel_path, None)
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
        assert!(matches!(err, NerveError::PathTraversal(_)));
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
