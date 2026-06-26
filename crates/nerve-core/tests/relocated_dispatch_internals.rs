//! Relocated fs-atomic dispatch tests that reach the dispatch-internal edit
//! applier (`apply_changes`) + `DiffOptions`. They drive `nerve_fs::FsCatalogProvider`
//! (so they cannot stay in an in-src `#[cfg(test)]` module) AND need kernel
//! internals, so they go through the gated `test_internals` re-export and only
//! build/run with `--features test-internals`. Bodies are verbatim from the old
//! in-src `dispatch::tests::editing` module.
#![cfg(feature = "test-internals")]

use nerve_core::edit;
use nerve_core::test_internals::{DiffOptions, apply_changes};
use nerve_core::{DispatchError, NerveError, RootPolicy};
use nerve_fs::{FsCatalogProvider, ScanOptions};
use std::fs;

fn provider_for(dir: &std::path::Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    )
}

#[test]
fn apply_patch_duplicate_create_fails_preflight_without_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "new.txt".to_string(),
            content: "one\n".to_string(),
        },
        edit::FileChange::Create {
            path: "new.txt".to_string(),
            content: "two\n".to_string(),
        },
    ];
    let err = apply_changes(&provider, changes, DiffOptions::default(), false)
        .err()
        .expect("duplicate create preflight");
    assert!(err.to_string().contains("duplicate create/update target"));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert!(!dir.path().join("new.txt").exists());
}

#[test]
fn create_over_existing_fails_preflight_before_later_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::write(dir.path().join("exists.txt"), "old\n").expect("seed exists");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Create {
            path: "exists.txt".to_string(),
            content: "new\n".to_string(),
        },
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
    ];
    let err = apply_changes(&provider, changes, DiffOptions::default(), false)
        .err()
        .expect("existing destination preflight");
    assert!(err.to_string().contains("destination already exists"));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("exists.txt")).unwrap(),
        "old\n"
    );
}

#[test]
fn non_atomic_preflight_rejects_invalid_create_before_update() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::write(dir.path().join("not_dir"), "file\n").expect("seed blocker");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "not_dir/new.txt".to_string(),
            content: "new\n".to_string(),
        },
    ];
    apply_changes(&provider, changes, DiffOptions::default(), false)
        .err()
        .expect("invalid destination preflight");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
}

#[test]
fn fs_atomic_backup_collision_preserves_user_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::create_dir(dir.path().join("existing_dir")).expect("seed blocker");
    let backup_name = format!(".a.txt.ctx-bak-{}-0-0", std::process::id());
    fs::write(dir.path().join(&backup_name), "do not delete\n").expect("seed backup collision");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "existing_dir".to_string(),
            content: "new\n".to_string(),
        },
    ];
    apply_changes(&provider, changes, DiffOptions::default(), true)
        .err()
        .expect("atomic rollback");
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(backup_name)).unwrap(),
        "do not delete\n"
    );
}

#[test]
fn fs_atomic_rollback_restores_first_write_after_later_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "alpha\n").expect("seed a");
    fs::create_dir(dir.path().join("existing_dir")).expect("seed blocker");
    let provider = provider_for(dir.path());
    let changes = vec![
        edit::FileChange::Update {
            path: "a.txt".to_string(),
            content: "ALPHA\n".to_string(),
        },
        edit::FileChange::Create {
            path: "existing_dir".to_string(),
            content: "new\n".to_string(),
        },
    ];
    let err = apply_changes(&provider, changes, DiffOptions::default(), true)
        .err()
        .expect("atomic rollback");
    assert!(matches!(
        err,
        DispatchError::Core(NerveError::AtomicBatchFailed { .. })
    ));
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert!(dir.path().join("existing_dir").is_dir());
}
