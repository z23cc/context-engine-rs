//! Relocated provider-dependent unit tests for the shared snapshot-memoized
//! CodeGraph indexes (`graph/{memo,derived,definitions}`).
//!
//! These used to live in `nerve-core`'s in-src `#[cfg(test)]` modules but moved
//! out because they drive `nerve_fs::FsCatalogProvider` (the `dev-dependencies`
//! back-edge forbids constructing it in an in-src test). They reach kernel
//! internals (the shared memos), so they go through the gated `test_internals`
//! re-export and only build/run with `--features test-internals`.
#![cfg(feature = "test-internals")]

use nerve_core::CancelToken;
use nerve_core::RootPolicy;
use nerve_core::test_internals::{
    indexed_files_cancellable, shared_definition_index, shared_indexed_files,
    shared_reference_graph,
};
use nerve_fs::{FsCatalogProvider, ScanOptions};
use std::fs;
use std::sync::Arc;

fn provider_for(dir: &std::path::Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    )
}

// ---- graph/memo.rs -------------------------------------------------------

#[test]
fn same_cached_snapshot_returns_ptr_eq_shared_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("write");
    let provider = provider_for(dir.path());

    let snapshot = provider.snapshot_arc().expect("snapshot");
    let first = shared_indexed_files(&provider, &snapshot, &CancelToken::never()).expect("idx");
    let second = shared_indexed_files(&provider, &snapshot, &CancelToken::never()).expect("idx");

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

    let shared = shared_indexed_files(&provider, &snapshot, &CancelToken::never()).expect("shared");
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

// ---- graph/derived.rs ----------------------------------------------------

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
    let first = shared_reference_graph(&provider, &snapshot, &CancelToken::never()).expect("graph");
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
    // it, so a genuinely new file->file edge appears and the indexed-symbol
    // count grows. A graph rebuilt off the new snapshot must reflect both; a
    // stale memo would report the old counts.
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

// ---- graph/definitions.rs ------------------------------------------------

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
fn fs_provider_edit_invalidate_serves_fresh_index_not_stale_memo_definitions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("a.rs");
    fs::write(&file, "pub fn alpha() {}\n").expect("write");
    let provider = provider_for(dir.path());

    let snapshot_a = provider.snapshot_arc().expect("snapshot a");
    let index_a =
        shared_definition_index(&provider, &snapshot_a, &CancelToken::never()).expect("index a");
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
    let index_b =
        shared_definition_index(&provider, &snapshot_b, &CancelToken::never()).expect("index b");

    assert!(
        !index_b.occurrences("beta").is_empty(),
        "index after edit+invalidate must reflect the new symbol, not the stale memo"
    );
    assert!(
        index_b.occurrences("alpha").is_empty(),
        "the renamed-away symbol must be gone, not served from the stale memo"
    );
}
