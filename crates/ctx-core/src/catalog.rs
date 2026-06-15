//! Filesystem-backed catalog provider.

use crate::{
    cancel::CancelToken,
    codemap::symbols_for_path,
    models::*,
    port::{CatalogProvider, CodeSymbolsResult},
    security::RootPolicy,
    snapshot::CatalogSnapshot,
};
use ignore::WalkBuilder;
use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};

/// Options controlling catalog scan cost.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub max_entries: usize,
    pub snapshot_cache_ttl: Duration,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            snapshot_cache_ttl: Duration::from_millis(1_000),
        }
    }
}

type Clock = dyn Fn() -> Instant + Send + Sync;

/// Filesystem provider using an allow-listed root policy.
#[derive(Clone)]
pub struct FsCatalogProvider {
    policy: RootPolicy,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    modified: Option<SystemTime>,
    size: u64,
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
            let mut builder = WalkBuilder::new(&root.path);
            let filter_cancel = cancel.clone();
            builder
                .hidden(false)
                .git_ignore(true)
                .git_exclude(true)
                .parents(true)
                .filter_entry(move |entry| {
                    if filter_cancel.is_cancelled() {
                        return false;
                    }
                    let name = entry.file_name().to_string_lossy();
                    !matches!(name.as_ref(), ".git" | "node_modules" | ".build" | "target")
                });

            let root_path = root.path.clone();
            let root_id = root.id.clone();
            let entries = Arc::clone(&entries);
            let diagnostics = Arc::clone(&diagnostics);
            let cancel = cancel.clone();

            builder.build_parallel().run(|| {
                let root_path = root_path.clone();
                let root_id = root_id.clone();
                let entries = Arc::clone(&entries);
                let diagnostics = Arc::clone(&diagnostics);
                let cancel = cancel.clone();

                Box::new(move |dent| {
                    if cancel.is_cancelled() {
                        return ignore::WalkState::Quit;
                    }
                    let dent = match dent {
                        Ok(dent) => dent,
                        Err(err) => {
                            diagnostics
                                .lock()
                                .expect("diagnostics lock")
                                .push(Diagnostic {
                                    path: None,
                                    message: err.to_string(),
                                });
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
                            diagnostics
                                .lock()
                                .expect("diagnostics lock")
                                .push(Diagnostic {
                                    path: Some(path.to_path_buf()),
                                    message: err.to_string(),
                                });
                            return ignore::WalkState::Continue;
                        }
                    };
                    let rel_path = path
                        .strip_prefix(&root_path)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    entries.lock().expect("entries lock").push(CatalogEntry {
                        root_id: root_id.clone(),
                        rel_path,
                        abs_path: path.to_path_buf(),
                        size: metadata.len(),
                    });
                    ignore::WalkState::Continue
                })
            });
            cancel.check_cancelled()?;
        }

        cancel.check_cancelled()?;
        let mut entries = Arc::try_unwrap(entries)
            .expect("entries references dropped")
            .into_inner()
            .expect("entries lock");
        if entries.len() > self.options.max_entries {
            return Err(CtxError::EntryLimitExceeded {
                limit: self.options.max_entries,
            });
        }
        entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        let mut diagnostics = Arc::try_unwrap(diagnostics)
            .expect("diagnostics references dropped")
            .into_inner()
            .expect("diagnostics lock");
        diagnostics.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.message.cmp(&right.message))
        });
        Ok(CatalogSnapshot {
            generation: 1,
            roots: self.policy.roots().to_vec(),
            entries,
            diagnostics,
        })
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

fn cache_entry_fresh(now: Instant, created_at: Instant, ttl: Duration) -> bool {
    now.checked_duration_since(created_at)
        .is_some_and(|age| age <= ttl)
}

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

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CtxError> {
        let allowed = self.policy.resolve_allowed(path)?;
        fs::read(&allowed).map_err(|err| CtxError::io(allowed, err))
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
        let parsed: CodeSymbolsResult = symbols_for_path(&source, rel_path)
            .map(|maybe| maybe.map(|(language, symbols)| (language, Arc::new(symbols))));
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
    fn snapshot_cache_reuses_arc_until_ttl_expires() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("first.rs"), "pub fn first() {}\n").expect("write");
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock_now = Arc::clone(&now);
        let provider = FsCatalogProvider::with_clock(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions {
                snapshot_cache_ttl: Duration::from_millis(1_000),
                ..ScanOptions::default()
            },
            move || *clock_now.lock().expect("clock lock"),
        );

        let first = provider.snapshot_arc().expect("snapshot");
        fs::write(dir.path().join("second.rs"), "pub fn second() {}\n").expect("write");
        let second = provider.snapshot_arc().expect("cached snapshot");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(paths(&second), vec!["first.rs"]);

        *now.lock().expect("clock lock") += Duration::from_millis(1_001);
        let third = provider.snapshot_arc().expect("expired snapshot");
        assert!(!Arc::ptr_eq(&first, &third));
        assert_eq!(paths(&third), vec!["first.rs", "second.rs"]);
    }

    #[test]
    fn invalidate_forces_snapshot_rebuild() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("first.rs"), "pub fn first() {}\n").expect("write");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );

        let first = provider.snapshot_arc().expect("snapshot");
        fs::write(dir.path().join("second.rs"), "pub fn second() {}\n").expect("write");
        provider.invalidate();
        let second = provider.snapshot_arc().expect("rebuilt snapshot");

        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(paths(&second), vec!["first.rs", "second.rs"]);
    }

    #[test]
    fn codemap_cache_reuses_symbols_until_signature_changes_or_invalidates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lib.rs");
        fs::write(&path, "pub fn first() {}\n").expect("write");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );

        let first = provider
            .code_symbols_for_path(&path, "lib.rs")
            .expect("read")
            .expect("parse")
            .expect("supported")
            .1;
        let second = provider
            .code_symbols_for_path(&path, "lib.rs")
            .expect("read")
            .expect("parse")
            .expect("supported")
            .1;
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(provider.codemap_cache_len(), 1);

        fs::write(&path, "pub fn first() {}\npub fn second() {}\n").expect("rewrite");
        let stale = provider
            .code_symbols_for_path(&path, "lib.rs")
            .expect("read")
            .expect("parse")
            .expect("supported")
            .1;
        assert!(!Arc::ptr_eq(&first, &stale));
        assert_eq!(stale.len(), 2);

        provider.invalidate();
        assert_eq!(provider.codemap_cache_len(), 0);
        let after_invalidate = provider
            .code_symbols_for_path(&path, "lib.rs")
            .expect("read")
            .expect("parse")
            .expect("supported")
            .1;
        assert!(!Arc::ptr_eq(&stale, &after_invalidate));
    }
}
