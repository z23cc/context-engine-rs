//! Process-global, snapshot-identity-memoized shared index.
//!
//! Every navigation / `build_context` call used to re-run
//! [`indexed_files_cancellable`] from scratch — re-reading bytes, re-collecting
//! symbols, and re-sorting a fresh `Vec<IndexedFile>` even though the underlying
//! parses are already codemap-cached by the provider. This module builds that
//! `Vec<IndexedFile>` **once per snapshot** and reuses it across all callers for
//! as long as the provider keeps serving the same cached snapshot `Arc`.
//!
//! ## Memo key (the correctness crux)
//!
//! The memo is keyed on **snapshot `Arc` identity**, never on
//! `CatalogSnapshot.generation`. `FsCatalogProvider` hard-codes
//! `generation = 1` permanently (`catalog/fs_scan.rs` `finalize_snapshot`); the
//! real edit counter lives in a separate `ProviderCache.generation` that never
//! reaches the snapshot value tools observe. A generation-keyed memo would
//! therefore serve a **stale** index after an edit — a determinism violation.
//!
//! The provider re-serves the *same* `Arc<CatalogSnapshot>` within its cache TTL
//! and drops it to `None` on every write/delete/rename (`invalidate()`), so the
//! next call after an edit builds a fresh `Arc`. We store a [`Weak`] reference to
//! that `Arc` and confirm a hit with [`Arc::ptr_eq`] — this avoids the ABA
//! problem of raw-pointer keys and needs no content hashing. The strong `Arc` is
//! deliberately *not* stored, so the memo never pins snapshots in memory.
//!
//! The memo is a pure derived cache: a hit is byte-identical to a miss, because
//! both return the exact output of [`indexed_files_cancellable`] for the same
//! snapshot. The bound below keeps it from growing without limit.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::{
    cancel::CancelToken, models::NerveError, port::CatalogProvider, repomap::IndexedFile,
    repomap::indexed_files_cancellable, snapshot::CatalogSnapshot,
};

/// Maximum number of distinct snapshots whose shared index is retained at once.
///
/// Small on purpose: a daemon usually drives one or a few live snapshots
/// concurrently (one per active workspace). Entries are held by `Weak`, so an
/// over-budget eviction only drops the memoized `Vec`, never a live snapshot.
const SHARED_INDEX_CAP: usize = 8;

struct CacheEntry {
    /// Identity key: upgraded + `Arc::ptr_eq`'d against the caller's snapshot.
    snapshot: Weak<CatalogSnapshot>,
    /// The memoized, fully-sorted indexed file set for that snapshot.
    indexed: Arc<Vec<IndexedFile>>,
}

/// MRU-ordered (front = most recently used) bounded list of memo entries.
#[derive(Default)]
struct SharedIndexCache {
    entries: Vec<CacheEntry>,
}

fn cache() -> &'static Mutex<SharedIndexCache> {
    static CACHE: OnceLock<Mutex<SharedIndexCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SharedIndexCache::default()))
}

/// Return the memoized indexed-file set for `snapshot`, building it once on a
/// miss via the existing [`indexed_files_cancellable`] logic.
///
/// On a hit the returned `Arc` is `Arc::ptr_eq`-equal across repeated calls that
/// observe the same cached snapshot `Arc`. The build runs **without** holding the
/// cache lock so concurrent callers on different snapshots never serialize on the
/// O(repo) parse pass; a racing duplicate build is reconciled on re-check.
pub(crate) fn shared_indexed_files<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    cancel: &CancelToken,
) -> Result<Arc<Vec<IndexedFile>>, NerveError> {
    if let Some(indexed) = lookup(snapshot) {
        return Ok(indexed);
    }

    // Build outside the lock: O(repo) work must not block other snapshots.
    let built = Arc::new(indexed_files_cancellable(provider, snapshot, cancel)?);
    Ok(insert(snapshot, built))
}

/// Look up `snapshot` by `Arc` identity, pruning any dead `Weak` entries seen.
/// Promotes a hit to the front (MRU).
fn lookup(snapshot: &Arc<CatalogSnapshot>) -> Option<Arc<Vec<IndexedFile>>> {
    let mut cache = crate::sync::lock_recover(cache());
    let mut hit = None;
    let mut index = 0;
    while index < cache.entries.len() {
        match cache.entries[index].snapshot.upgrade() {
            Some(existing) if Arc::ptr_eq(&existing, snapshot) => {
                hit = Some(index);
                break;
            }
            Some(_) => index += 1,
            // Dead entry: the snapshot was dropped (e.g. after an edit). Prune it.
            None => {
                cache.entries.remove(index);
            }
        }
    }
    let index = hit?;
    let entry = cache.entries.remove(index);
    let indexed = Arc::clone(&entry.indexed);
    cache.entries.insert(0, entry);
    Some(indexed)
}

/// Insert (or reconcile a racing duplicate of) the built index, returning the
/// canonical `Arc` so concurrent builders converge on a single shared value.
fn insert(snapshot: &Arc<CatalogSnapshot>, built: Arc<Vec<IndexedFile>>) -> Arc<Vec<IndexedFile>> {
    let mut cache = crate::sync::lock_recover(cache());
    // Another thread may have inserted the same snapshot while we built ours.
    if let Some(existing) = take_existing(&mut cache, snapshot) {
        cache.entries.insert(
            0,
            CacheEntry {
                snapshot: Arc::downgrade(snapshot),
                indexed: Arc::clone(&existing),
            },
        );
        return existing;
    }
    cache.entries.insert(
        0,
        CacheEntry {
            snapshot: Arc::downgrade(snapshot),
            indexed: Arc::clone(&built),
        },
    );
    evict_over_budget(&mut cache);
    built
}

/// Remove and return the memoized index for `snapshot` if present, pruning dead
/// entries along the way.
fn take_existing(
    cache: &mut SharedIndexCache,
    snapshot: &Arc<CatalogSnapshot>,
) -> Option<Arc<Vec<IndexedFile>>> {
    let mut index = 0;
    while index < cache.entries.len() {
        match cache.entries[index].snapshot.upgrade() {
            Some(existing) if Arc::ptr_eq(&existing, snapshot) => {
                return Some(cache.entries.remove(index).indexed);
            }
            Some(_) => index += 1,
            None => {
                cache.entries.remove(index);
            }
        }
    }
    None
}

fn evict_over_budget(cache: &mut SharedIndexCache) {
    while cache.entries.len() > SHARED_INDEX_CAP {
        cache.entries.pop();
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::catalog::{FsCatalogProvider, ScanOptions};
    use crate::security::RootPolicy;
    use std::fs;

    fn provider_for(dir: &std::path::Path) -> FsCatalogProvider {
        FsCatalogProvider::new(
            RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        )
    }

    #[test]
    fn same_cached_snapshot_returns_ptr_eq_shared_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("write");
        let provider = provider_for(dir.path());

        let snapshot = provider.snapshot_arc().expect("snapshot");
        let first = shared_indexed_files(&provider, &snapshot, &CancelToken::never()).expect("idx");
        let second =
            shared_indexed_files(&provider, &snapshot, &CancelToken::never()).expect("idx");

        // Hit path: identical snapshot Arc -> the very same memoized vec.
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated calls on the same snapshot Arc must reuse the memoized index"
        );
    }

    #[test]
    fn fs_provider_edit_invalidate_serves_fresh_index_not_stale_memo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("lib.rs");
        fs::write(&path, "pub fn alpha() {}\n").expect("write");
        let provider = provider_for(dir.path());

        // First snapshot + shared index reflects the original symbol set.
        let snapshot_a = provider.snapshot_arc().expect("snapshot a");
        let index_a =
            shared_indexed_files(&provider, &snapshot_a, &CancelToken::never()).expect("idx a");
        let names_a: Vec<&str> = index_a
            .iter()
            .flat_map(|file| file.symbols.iter())
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert!(names_a.contains(&"alpha"));
        assert!(!names_a.contains(&"beta"));

        // Edit the file and invalidate exactly as the provider's write path does,
        // dropping the cached snapshot Arc so the next call builds a fresh one.
        fs::write(&path, "pub fn alpha() {}\npub fn beta() {}\n").expect("rewrite");
        provider.invalidate();

        // Second snapshot is a brand-new Arc (not ptr_eq to snapshot_a) so the
        // memo must miss and rebuild — never serve the stale index (hit == miss).
        let snapshot_b = provider.snapshot_arc().expect("snapshot b");
        assert!(
            !Arc::ptr_eq(&snapshot_a, &snapshot_b),
            "invalidate must force a fresh snapshot Arc after an edit"
        );
        let index_b =
            shared_indexed_files(&provider, &snapshot_b, &CancelToken::never()).expect("idx b");
        let names_b: Vec<&str> = index_b
            .iter()
            .flat_map(|file| file.symbols.iter())
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert!(
            names_b.contains(&"beta"),
            "shared index after edit+invalidate must reflect the new symbol, not the stale memo"
        );
    }

    #[test]
    fn shared_index_matches_direct_indexed_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("a.rs"),
            "pub fn one() { two(); }\npub fn two() {}\n",
        )
        .expect("write a");
        fs::write(dir.path().join("b.rs"), "pub struct Widget;\n").expect("write b");
        let provider = provider_for(dir.path());
        let snapshot = provider.snapshot_arc().expect("snapshot");

        let shared =
            shared_indexed_files(&provider, &snapshot, &CancelToken::never()).expect("shared");
        let direct =
            indexed_files_cancellable(&provider, &snapshot, &CancelToken::never()).expect("direct");

        // Parity: the memoized vec is byte-identical to a fresh per-call build.
        assert_eq!(shared.len(), direct.len());
        for (memoized, fresh) in shared.iter().zip(direct.iter()) {
            assert_eq!(memoized.path, fresh.path);
            assert_eq!(memoized.symbols.len(), fresh.symbols.len());
            assert_eq!(memoized.references.len(), fresh.references.len());
        }
    }
}
