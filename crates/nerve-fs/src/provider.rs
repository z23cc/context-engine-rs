//! Native filesystem catalog provider.
//!
//! `FsCatalogProvider` keeps the filesystem + ignore walker behind the kernel's
//! `CatalogProvider` port. It is intentionally **out of** the determinism kernel
//! (`nerve-core`): everything impure the kernel forbids — wall-clock (`Instant`),
//! `SystemTime` signatures, background `std::thread` codemap warming — lives here,
//! host-side (architecture-north-star §3.1 / INV-R2).

use crate::scan::{finalize_snapshot, scan_root};
use nerve_core::port::{CatalogProvider, CodeSymbolsResult, FileSignature};
use nerve_core::{
    CancelToken, CatalogSnapshot, NerveError, RootPolicy, RootRef, Selection, edit::FileChange,
    language_name_for_path, parse_symbols_for_path, sync,
};
use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, Weak,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Maximum entries returned in a snapshot; scans above this size are truncated with a diagnostic.
    pub max_entries: usize,
    pub snapshot_cache_ttl: Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            snapshot_cache_ttl: Duration::from_millis(5_000),
        }
    }
}

type Clock = dyn Fn() -> Instant + Send + Sync;

/// Filesystem provider using an allow-listed root policy.
#[derive(Clone)]
pub struct FsCatalogProvider {
    pub(crate) policy: RootPolicy,
    options: ScanOptions,
    cache: Arc<ProviderCache>,
    clock: Arc<Clock>,
}

impl fmt::Debug for FsCatalogProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsCatalogProvider")
            .field("policy", &self.policy)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct ProviderCache {
    snapshot: RwLock<Option<CachedSnapshot>>,
    codemap: RwLock<HashMap<PathBuf, CachedCodeSymbols>>,
    selection: RwLock<Selection>,
    codemap_warming: Mutex<Option<CodemapWarmInProgress>>,
    generation: AtomicU64,
}

#[derive(Debug, Clone)]
struct CodemapWarmInProgress {
    generation: u64,
    cancel: CancelToken,
}

#[derive(Debug, Clone)]
struct CachedSnapshot {
    created_at: Instant,
    snapshot: Arc<CatalogSnapshot>,
}

#[derive(Debug, Clone)]
struct CachedCodeSymbols {
    signature: FileSignature,
    symbols: Arc<CodeSymbolsResult>,
}

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

        let mut guard = sync::write_recover(&self.cache.snapshot);
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
        *sync::write_recover(&self.cache.snapshot) = None;
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
        if let Some(warming) = sync::lock_recover(&self.cache.codemap_warming).take() {
            warming.cancel.cancel();
        }
    }

    #[must_use]
    pub fn roots(&self) -> &[RootRef] {
        self.policy.roots()
    }

    fn cached_snapshot(&self, now: Instant) -> Option<Arc<CatalogSnapshot>> {
        let guard = sync::read_recover(&self.cache.snapshot);
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
            .all(|entry| language_name_for_path(&entry.rel_path).is_none())
        {
            return;
        }
        let cancel = CancelToken::never();
        {
            let mut warming = sync::lock_recover(&self.cache.codemap_warming);
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
                let mut warming = sync::lock_recover(&cache.codemap_warming);
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
            if language_name_for_path(&entry.rel_path).is_none() {
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
        if let Some(cached) = sync::read_recover(&cache.codemap)
            .get(allowed)
            .filter(|cached| cached.signature == signature)
        {
            return Ok((*cached.symbols).clone());
        }

        let bytes = fs::read(allowed).map_err(|err| NerveError::io(allowed, err))?;
        let source = String::from_utf8_lossy(&bytes);
        let parsed: CodeSymbolsResult = parse_symbols_for_path(&source, rel_path);
        let generation_current = |expected_generation: Option<u64>| {
            expected_generation.is_none_or(|generation| {
                cache.generation.load(AtomicOrdering::SeqCst) == generation
            })
        };
        if generation_current(expected_generation) {
            let mut codemap = sync::write_recover(&cache.codemap);
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

fn cache_entry_fresh(now: Instant, created_at: Instant, ttl: Duration) -> bool {
    now.checked_duration_since(created_at)
        .is_some_and(|age| age <= ttl)
}

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
        sync::read_recover(&self.cache.selection).clone()
    }

    fn set_selection(&self, selection: Selection) {
        *sync::write_recover(&self.cache.selection) = selection;
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

    fn apply_file_batch(&self, changes: &[FileChange], atomic: bool) -> Result<(), NerveError> {
        if !atomic {
            for change in changes {
                crate::atomic::apply_change(self, change)?;
            }
            self.invalidate();
            return Ok(());
        }
        crate::atomic::apply_atomic_batch(self, changes)
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
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use std::thread;
    use std::time::Duration as StdDuration;

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
    fn invalidation_retains_signature_validated_codemap_cache() {
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

        // invalidate() drops the snapshot (forcing a fresh scan) but RETAINS the
        // signature-validated parse cache, so unchanged files are not re-parsed on the
        // next query — incremental parsing across edits / TTL lapses.
        provider.invalidate();
        assert_eq!(provider.codemap_cache_len(), 1);

        // A content change is still reflected: the (mtime,size) signature mismatches,
        // so the next read re-parses rather than serving the stale cached symbols.
        fs::write(&path, "pub fn one() {}\npub fn two() {}\n").expect("rewrite");
        let parsed = provider
            .code_symbols_for_path(&path, "lib.rs")
            .expect("codemap")
            .expect("parse")
            .expect("symbols");
        assert!(
            parsed.symbols.iter().any(|symbol| symbol.name == "two"),
            "edit must be reflected via signature mismatch, not served stale"
        );
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
