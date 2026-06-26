//! Process-global, snapshot-memoized shared indexed-file set (CodeGraph T0).
//!
//! Every navigation / `build_context` call used to re-run
//! [`indexed_files_cancellable`] from scratch — re-reading bytes, re-collecting
//! symbols, and re-sorting a fresh `Vec<IndexedFile>` even though the underlying
//! parses are already codemap-cached by the provider. This builds that
//! `Vec<IndexedFile>` **once per snapshot** and reuses it across all callers for
//! as long as the provider keeps serving the same cached snapshot `Arc`.
//!
//! Keying, eviction, and the hit==miss determinism guarantee live in
//! [`super::snapshot_memo`]; this module is just the indexed-file instantiation.

use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    cancel::CancelToken,
    models::NerveError,
    port::CatalogProvider,
    repomap::{IndexedFile, indexed_files_cancellable},
    snapshot::CatalogSnapshot,
};

use super::snapshot_memo::SnapshotMemo;

/// Maximum number of distinct snapshots whose shared index is retained at once.
/// Small on purpose: a daemon usually drives one or a few live snapshots.
const SHARED_INDEX_CAP: usize = 8;

fn cache() -> &'static Mutex<SnapshotMemo<Vec<IndexedFile>>> {
    static CACHE: OnceLock<Mutex<SnapshotMemo<Vec<IndexedFile>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SnapshotMemo::new(SHARED_INDEX_CAP)))
}

/// Return the memoized indexed-file set for `snapshot`, building it once on a
/// miss via the existing [`indexed_files_cancellable`] logic. A hit is
/// byte-identical to a miss (same snapshot → same output).
pub fn shared_indexed_files<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    cancel: &CancelToken,
) -> Result<Arc<Vec<IndexedFile>>, NerveError> {
    SnapshotMemo::get_or_build(cache(), snapshot, || {
        indexed_files_cancellable(provider, snapshot, cancel)
    })
}
