//! Process-global, snapshot-identity-memoized derived reference graph.
//!
//! `get_repo_map` (and `build_context`, which calls it) used to rebuild the
//! whole cross-file [`ReferenceGraph`] — an O(edges) pass over every reference
//! in the repo — on **every** call, even when the underlying snapshot and its
//! shared [`IndexedFile`] set were unchanged. This module memoizes that derived
//! graph **once per snapshot** and reuses it for as long as the provider keeps
//! serving the same cached snapshot `Arc`.
//!
//! ## Why this is byte-identical (the determinism crux)
//!
//! [`ReferenceGraph::build`] is a **pure** function of `&[IndexedFile]`: it reads
//! only `file.references`, `file.symbols`, and `language_family`, never
//! `query_match`. The shared index from [`shared_indexed_files`] is the exact
//! `Indexed`-filtered, path-sorted set `get_repo_map` would build from its own
//! `analyze_files_cancellable` pass — the only per-file difference a query makes
//! is the `query_match` bool, which the graph build ignores. So a graph built off
//! `shared_indexed_files` is identical to the per-call graph; caching a pure
//! function's output is byte-identical by construction.
//!
//! ## Memo key
//!
//! Identical to [`super::memo`]: keyed on snapshot **`Arc` identity** via a
//! [`Weak`] reference confirmed with [`Arc::ptr_eq`], never on
//! `CatalogSnapshot.generation` (frozen at 1 on `FsCatalogProvider`). The
//! provider drops its cached snapshot `Arc` on every edit (`invalidate()`), so
//! the next call builds a fresh `Arc` and the memo misses — never serving a stale
//! graph. The strong `Arc` is not stored, so the memo never pins snapshots.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::{
    cancel::CancelToken, models::NerveError, port::CatalogProvider, repomap::ReferenceGraph,
    snapshot::CatalogSnapshot,
};

use super::shared_indexed_files;

/// Maximum number of distinct snapshots whose reference graph is retained at
/// once. Mirrors `SHARED_INDEX_CAP`: a daemon usually drives one or a few live
/// snapshots concurrently, and `Weak` keys mean an over-budget eviction only
/// drops the memoized graph, never a live snapshot.
const SHARED_GRAPH_CAP: usize = 8;

struct CacheEntry {
    /// Identity key: upgraded + `Arc::ptr_eq`'d against the caller's snapshot.
    snapshot: Weak<CatalogSnapshot>,
    /// The memoized reference graph derived from that snapshot's shared index.
    graph: Arc<ReferenceGraph>,
}

/// MRU-ordered (front = most recently used) bounded list of memo entries.
#[derive(Default)]
struct SharedGraphCache {
    entries: Vec<CacheEntry>,
}

fn cache() -> &'static Mutex<SharedGraphCache> {
    static CACHE: OnceLock<Mutex<SharedGraphCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SharedGraphCache::default()))
}

/// Return the memoized [`ReferenceGraph`] for `snapshot`, building it once on a
/// miss from the shared indexed-file set.
///
/// On a hit the returned `Arc` is `Arc::ptr_eq`-equal across repeated calls that
/// observe the same cached snapshot `Arc`. The build runs **without** holding the
/// cache lock so concurrent callers on different snapshots never serialize on the
/// O(edges) graph construction; a racing duplicate build is reconciled on
/// re-check.
pub(crate) fn shared_reference_graph<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    cancel: &CancelToken,
) -> Result<Arc<ReferenceGraph>, NerveError> {
    if let Some(graph) = lookup(snapshot) {
        return Ok(graph);
    }

    // Build outside the lock: O(edges) work must not block other snapshots. The
    // shared index is itself memoized, so a graph miss reuses the cached files.
    let files = shared_indexed_files(provider, snapshot, cancel)?;
    let built = Arc::new(ReferenceGraph::build_cancellable(&files, cancel)?);
    Ok(insert(snapshot, built))
}

/// Look up `snapshot` by `Arc` identity, pruning any dead `Weak` entries seen.
/// Promotes a hit to the front (MRU).
fn lookup(snapshot: &Arc<CatalogSnapshot>) -> Option<Arc<ReferenceGraph>> {
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
    let graph = Arc::clone(&entry.graph);
    cache.entries.insert(0, entry);
    Some(graph)
}

/// Insert (or reconcile a racing duplicate of) the built graph, returning the
/// canonical `Arc` so concurrent builders converge on a single shared value.
fn insert(snapshot: &Arc<CatalogSnapshot>, built: Arc<ReferenceGraph>) -> Arc<ReferenceGraph> {
    let mut cache = crate::sync::lock_recover(cache());
    // Another thread may have inserted the same snapshot while we built ours.
    if let Some(existing) = take_existing(&mut cache, snapshot) {
        cache.entries.insert(
            0,
            CacheEntry {
                snapshot: Arc::downgrade(snapshot),
                graph: Arc::clone(&existing),
            },
        );
        return existing;
    }
    cache.entries.insert(
        0,
        CacheEntry {
            snapshot: Arc::downgrade(snapshot),
            graph: Arc::clone(&built),
        },
    );
    evict_over_budget(&mut cache);
    built
}

/// Remove and return the memoized graph for `snapshot` if present, pruning dead
/// entries along the way.
fn take_existing(
    cache: &mut SharedGraphCache,
    snapshot: &Arc<CatalogSnapshot>,
) -> Option<Arc<ReferenceGraph>> {
    let mut index = 0;
    while index < cache.entries.len() {
        match cache.entries[index].snapshot.upgrade() {
            Some(existing) if Arc::ptr_eq(&existing, snapshot) => {
                return Some(cache.entries.remove(index).graph);
            }
            Some(_) => index += 1,
            None => {
                cache.entries.remove(index);
            }
        }
    }
    None
}

fn evict_over_budget(cache: &mut SharedGraphCache) {
    while cache.entries.len() > SHARED_GRAPH_CAP {
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
    fn same_cached_snapshot_returns_ptr_eq_reference_graph() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("a.rs"),
            "pub fn one() { two(); }\npub fn two() {}\n",
        )
        .expect("write");
        let provider = provider_for(dir.path());

        let snapshot = provider.snapshot_arc().expect("snapshot");
        let first =
            shared_reference_graph(&provider, &snapshot, &CancelToken::never()).expect("graph");
        let second =
            shared_reference_graph(&provider, &snapshot, &CancelToken::never()).expect("graph");

        // Hit path: identical snapshot Arc -> the very same memoized graph.
        assert!(
            Arc::ptr_eq(&first, &second),
            "repeated calls on the same snapshot Arc must reuse the memoized reference graph"
        );
    }

    #[test]
    fn fs_provider_edit_invalidate_serves_fresh_graph_not_stale_memo() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `caller.rs` references `make_target` defined in `target.rs`: one
        // file->file edge (caller -> target).
        let target = dir.path().join("target.rs");
        let caller = dir.path().join("caller.rs");
        let extra = dir.path().join("extra.rs");
        fs::write(&target, "pub fn make_target() -> usize { 1 }\n").expect("write target");
        fs::write(&caller, "pub fn caller() -> usize { make_target() }\n").expect("write caller");
        let provider = provider_for(dir.path());

        // First snapshot + reference graph reflects the original edge set.
        let snapshot_a = provider.snapshot_arc().expect("snapshot a");
        let graph_a =
            shared_reference_graph(&provider, &snapshot_a, &CancelToken::never()).expect("graph a");
        let edges_a = graph_a.edge_count;
        let symbols_a = graph_a.symbols_indexed;
        assert!(edges_a >= 1, "expected at least the caller->target edge");

        // Add a brand-new defining file (`extra.rs`) and have `caller` call into
        // it, so a genuinely new file->file edge (caller -> extra) appears and the
        // indexed-symbol count grows. A graph rebuilt off the new snapshot must
        // reflect both; a stale memo would report the old counts.
        fs::write(&extra, "pub fn other() -> usize { 2 }\n").expect("write extra");
        fs::write(
            &caller,
            "pub fn caller() -> usize { make_target() + other() }\n",
        )
        .expect("rewrite caller");
        provider.invalidate();

        // Second snapshot is a brand-new Arc (not ptr_eq to snapshot_a), so the
        // memo must miss and rebuild — never serve the stale graph (hit == miss).
        let snapshot_b = provider.snapshot_arc().expect("snapshot b");
        assert!(
            !Arc::ptr_eq(&snapshot_a, &snapshot_b),
            "invalidate must force a fresh snapshot Arc after an edit"
        );
        let graph_b =
            shared_reference_graph(&provider, &snapshot_b, &CancelToken::never()).expect("graph b");

        assert!(
            graph_b.symbols_indexed > symbols_a,
            "reference graph after edit+invalidate must index the new symbol, not the stale memo \
             (was {symbols_a}, now {})",
            graph_b.symbols_indexed
        );
        assert!(
            graph_b.edge_count > edges_a,
            "reference graph after edit+invalidate must reflect the new edge, not the stale memo \
             (was {edges_a}, now {})",
            graph_b.edge_count
        );
    }
}
