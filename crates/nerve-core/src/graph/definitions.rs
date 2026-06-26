//! Process-global, snapshot-memoized inverted definition-name index.
//!
//! `find_references` re-scanned every file's every symbol to find the files
//! defining a queried name — `definition_file_indexes` and `count_definitions`
//! in `navigate/references.rs`, two O(repo·symbols) passes per call. This
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
//! Keying, eviction, and the hit==miss guarantee live in [`super::snapshot_memo`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::{
    cancel::CancelToken, models::NerveError, port::CatalogProvider, repomap::IndexedFile,
    snapshot::CatalogSnapshot,
};

use super::shared_indexed_files;
use super::snapshot_memo::SnapshotMemo;

/// Maximum number of distinct snapshots whose definition index is retained at
/// once. Mirrors the sibling memos' caps.
const SHARED_DEFINITION_INDEX_CAP: usize = 8;

/// Inverted index from a symbol name to the file indices of each defining
/// occurrence. Repeats encode multiple same-named symbols within one file, so a
/// consumer can recover either the defining-file set (dedup) or the symbol count
/// (with repeats) by applying its own per-file filter at lookup.
pub struct DefinitionNameIndex {
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
    pub fn occurrences(&self, name: &str) -> &[usize] {
        self.by_name.get(name).map_or(&[], Vec::as_slice)
    }
}

fn cache() -> &'static Mutex<SnapshotMemo<DefinitionNameIndex>> {
    static CACHE: OnceLock<Mutex<SnapshotMemo<DefinitionNameIndex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(SnapshotMemo::new(SHARED_DEFINITION_INDEX_CAP)))
}

/// Return the memoized [`DefinitionNameIndex`] for `snapshot`, building it once
/// on a miss from the shared indexed-file set.
pub fn shared_definition_index<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    cancel: &CancelToken,
) -> Result<Arc<DefinitionNameIndex>, NerveError> {
    SnapshotMemo::get_or_build(cache(), snapshot, || {
        let files = shared_indexed_files(provider, snapshot, cancel)?;
        cancel.check_cancelled()?;
        Ok(DefinitionNameIndex::build(&files))
    })
}
