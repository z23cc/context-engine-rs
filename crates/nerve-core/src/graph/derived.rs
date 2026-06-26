//! Process-global, snapshot-memoized derived reference graph.
//!
//! `get_repo_map` (and `build_context`, which calls it) used to rebuild the whole
//! cross-file [`ReferenceGraph`] — an O(edges) pass over every reference in the
//! repo — on **every** call, even when the snapshot and its shared
//! [`IndexedFile`] set were unchanged. This memoizes that derived graph **once per
//! snapshot** and reuses it while the provider serves the same cached `Arc`.
//!
//! ## Why this is byte-identical
//!
//! [`ReferenceGraph::build`] is a **pure** function of `&[IndexedFile]`: it reads
//! only `file.references`, `file.symbols`, and `language_family`, never
//! `query_match`. The shared index from [`shared_indexed_files`] is the exact
//! `Indexed`-filtered, path-sorted set `get_repo_map` would build itself, so a
//! graph built off it is identical to the per-call graph; caching a pure
//! function's output is byte-identical by construction.
//!
//! Keying, eviction, and the hit==miss guarantee live in [`super::snapshot_memo`].

use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    cancel::CancelToken, models::NerveError, port::CatalogProvider, repomap::ReferenceGraph,
    snapshot::CatalogSnapshot,
};

use super::shared_indexed_files;
use super::snapshot_memo::SnapshotMemo;

/// Maximum number of distinct snapshots whose reference graph is retained at
/// once. Mirrors the sibling memos' caps.
const SHARED_GRAPH_CAP: usize = 8;

fn cache() -> &'static Mutex<SnapshotMemo<ReferenceGraph>> {
    static CACHE: OnceLock<Mutex<SnapshotMemo<ReferenceGraph>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SnapshotMemo::new(SHARED_GRAPH_CAP)))
}

/// Return the memoized [`ReferenceGraph`] for `snapshot`, building it once on a
/// miss from the shared indexed-file set.
pub fn shared_reference_graph<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    cancel: &CancelToken,
) -> Result<Arc<ReferenceGraph>, NerveError> {
    SnapshotMemo::get_or_build(cache(), snapshot, || {
        let files = shared_indexed_files(provider, snapshot, cancel)?;
        ReferenceGraph::build_cancellable(&files, cancel)
    })
}
