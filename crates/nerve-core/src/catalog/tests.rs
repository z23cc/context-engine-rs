use super::*;
use std::{io::Write, sync::Mutex, thread, time::Duration as StdDuration};

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
