//! Process-global, snapshot-identity-memoized inverted definition-name index.
//!
//! `find_references` re-scanned every file's every symbol to find the files
//! defining a queried name — `definition_file_indexes` and `count_definitions`
//! in `navigate/references.rs`, two O(repo·symbols) passes per call. This module
//! memoizes a single inverted index `name -> [file index per matching symbol]`
//! **once per snapshot**, so repeated navigation over the same snapshot reuses it.
//!
//! ## Why this is byte-identical
//!
//! `references`'s definition predicate is exactly `symbol.name == request.symbol`
//! (with the per-file `language_matches` filter applied by the consumer). The
//! index records, per symbol name, the file index of each defining occurrence
//! (with repeats for multiple same-named symbols in one file). `references` then
//! applies the unchanged `language_matches` filter at lookup, so the resulting
//! file-index set (deduped into a `HashSet`) and symbol count (with repeats) are
//! identical to the prior full scans. Iteration order is irrelevant: the consumer
//! collects into a set and a count.
//!
//! Note: this index is intentionally UNPRUNED and name-only. It does NOT serve
//! `referencing_symbols`/`impact` (which add kind+path predicates) or repomap's
//! HDF-pruned `definition_index` byte-identically; those keep their own
//! derivation. It can later pre-filter their scans by name, but that is a
//! separate change.
//!
//! ## Memo key
//!
//! Identical to [`super::memo`] / [`super::derived`]: snapshot **`Arc` identity**
//! via a [`Weak`] confirmed with [`Arc::ptr_eq`], never `CatalogSnapshot.generation`
//! (frozen at 1 on `FsCatalogProvider`). The provider drops its cached snapshot
//! `Arc` on every edit, so the next call builds a fresh `Arc` and the memo misses
//! — never serving a stale index. The strong `Arc` is not stored.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::{
    cancel::CancelToken, models::NerveError, port::CatalogProvider, repomap::IndexedFile,
    snapshot::CatalogSnapshot,
};

use super::shared_indexed_files;

/// Maximum number of distinct snapshots whose definition index is retained at
/// once. Mirrors the sibling memos' caps.
const SHARED_DEFINITION_INDEX_CAP: usize = 8;

/// Inverted index from a symbol name to the file indices of each defining
/// occurrence. Repeats encode multiple same-named symbols within one file, so a
/// consumer can recover either the defining-file set (dedup) or the symbol count
/// (with repeats) by applying its own per-file filter at lookup.
pub(crate) struct DefinitionNameIndex {
    by_name: HashMap<String, Vec<usize>>,
}

impl DefinitionNameIndex {
    fn build(files: &[IndexedFile]) -> Self {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, file) in files.iter().enumerate() {
            for symbol in &file.symbols {
                by_name.entry(symbol.name.clone()).or_default().push(idx);
            }
        }
        Self { by_name }
    }

    /// File index of every symbol occurrence whose name is `name` (one entry per
    /// matching symbol, so a file with N same-named symbols appears N times);
    /// empty if the name defines nothing.
    pub(crate) fn occurrences(&self, name: &str) -> &[usize] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }
}

struct CacheEntry {
    snapshot: Weak<CatalogSnapshot>,
    index: Arc<DefinitionNameIndex>,
}

#[derive(Default)]
struct DefinitionIndexCache {
    entries: Vec<CacheEntry>,
}

fn cache() -> &'static Mutex<DefinitionIndexCache> {
    static CACHE: OnceLock<Mutex<DefinitionIndexCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(DefinitionIndexCache::default()))
}

/// Return the memoized [`DefinitionNameIndex`] for `snapshot`, building it once
/// on a miss from the shared indexed-file set. The build runs without holding the
/// cache lock; a racing duplicate is reconciled to one canonical `Arc`.
pub(crate) fn shared_definition_index<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    cancel: &CancelToken,
) -> Result<Arc<DefinitionNameIndex>, NerveError> {
    if let Some(index) = lookup(snapshot) {
        return Ok(index);
    }
    let files = shared_indexed_files(provider, snapshot, cancel)?;
    cancel.check_cancelled()?;
    let built = Arc::new(DefinitionNameIndex::build(&files));
    Ok(insert(snapshot, built))
}

/// Look up `snapshot` by `Arc` identity, pruning dead `Weak` entries; promotes a
/// hit to the front (MRU).
fn lookup(snapshot: &Arc<CatalogSnapshot>) -> Option<Arc<DefinitionNameIndex>> {
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
            None => {
                cache.entries.remove(index);
            }
        }
    }
    let index = hit?;
    let entry = cache.entries.remove(index);
    let result = Arc::clone(&entry.index);
    cache.entries.insert(0, entry);
    Some(result)
}

/// Insert (or reconcile a racing duplicate of) the built index, returning the
/// canonical `Arc` so concurrent builders converge on a single shared value.
fn insert(
    snapshot: &Arc<CatalogSnapshot>,
    built: Arc<DefinitionNameIndex>,
) -> Arc<DefinitionNameIndex> {
    let mut cache = crate::sync::lock_recover(cache());
    if let Some(existing) = take_existing(&mut cache, snapshot) {
        cache.entries.insert(
            0,
            CacheEntry {
                snapshot: Arc::downgrade(snapshot),
                index: Arc::clone(&existing),
            },
        );
        return existing;
    }
    cache.entries.insert(
        0,
        CacheEntry {
            snapshot: Arc::downgrade(snapshot),
            index: Arc::clone(&built),
        },
    );
    evict_over_budget(&mut cache);
    built
}

fn take_existing(
    cache: &mut DefinitionIndexCache,
    snapshot: &Arc<CatalogSnapshot>,
) -> Option<Arc<DefinitionNameIndex>> {
    let mut index = 0;
    while index < cache.entries.len() {
        match cache.entries[index].snapshot.upgrade() {
            Some(existing) if Arc::ptr_eq(&existing, snapshot) => {
                return Some(cache.entries.remove(index).index);
            }
            Some(_) => index += 1,
            None => {
                cache.entries.remove(index);
            }
        }
    }
    None
}

fn evict_over_budget(cache: &mut DefinitionIndexCache) {
    while cache.entries.len() > SHARED_DEFINITION_INDEX_CAP {
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
    fn same_cached_snapshot_returns_ptr_eq_definition_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "pub fn alpha() {}\n").expect("write");
        let provider = provider_for(dir.path());

        let snapshot = provider.snapshot_arc().expect("snapshot");
        let first =
            shared_definition_index(&provider, &snapshot, &CancelToken::never()).expect("index");
        let second =
            shared_definition_index(&provider, &snapshot, &CancelToken::never()).expect("index");

        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated calls on the same snapshot Arc must reuse the memoized definition index"
        );
    }

    #[test]
    fn occurrences_count_matches_repeated_symbols() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two symbols named `dup` in one file -> two occurrences at the same idx.
        fs::write(
            dir.path().join("a.rs"),
            "pub fn dup() {}\npub struct dup;\n",
        )
        .expect("write");
        let provider = provider_for(dir.path());
        let snapshot = provider.snapshot_arc().expect("snapshot");
        let index =
            shared_definition_index(&provider, &snapshot, &CancelToken::never()).expect("index");
        assert_eq!(
            index.occurrences("dup").len(),
            2,
            "each same-named symbol occurrence must be recorded (count semantics)"
        );
        assert!(index.occurrences("missing").is_empty());
    }

    #[test]
    fn fs_provider_edit_invalidate_serves_fresh_index_not_stale_memo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.rs");
        fs::write(&file, "pub fn alpha() {}\n").expect("write");
        let provider = provider_for(dir.path());

        let snapshot_a = provider.snapshot_arc().expect("snapshot a");
        let index_a = shared_definition_index(&provider, &snapshot_a, &CancelToken::never())
            .expect("index a");
        assert!(!index_a.occurrences("alpha").is_empty());
        assert!(index_a.occurrences("beta").is_empty());

        // Edit: rename the only symbol, invalidate, re-snapshot (fresh Arc).
        fs::write(&file, "pub fn beta() {}\n").expect("rewrite");
        provider.invalidate();
        let snapshot_b = provider.snapshot_arc().expect("snapshot b");
        assert!(
            !Arc::ptr_eq(&snapshot_a, &snapshot_b),
            "invalidate must force a fresh snapshot Arc after an edit"
        );
        let index_b = shared_definition_index(&provider, &snapshot_b, &CancelToken::never())
            .expect("index b");

        assert!(
            !index_b.occurrences("beta").is_empty(),
            "index after edit+invalidate must reflect the new symbol, not the stale memo"
        );
        assert!(
            index_b.occurrences("alpha").is_empty(),
            "the renamed-away symbol must be gone, not served from the stale memo"
        );
    }
}
