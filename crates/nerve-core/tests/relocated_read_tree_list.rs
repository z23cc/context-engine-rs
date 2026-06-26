//! Relocated provider-dependent unit tests for `read.rs`, `tree.rs`, and
//! `list_files.rs`.
//!
//! These used to live in `nerve-core`'s in-src `#[cfg(test)]` modules but moved
//! out because they drive `nerve_fs::FsCatalogProvider` (the `dev-dependencies`
//! back-edge forbids constructing it in an in-src test — it would compile
//! `nerve-core` twice, "multiple versions of crate `nerve_core`"). They only use
//! the public tool API, so no `test-internals` gate is needed.

use nerve_core::list_files::{ListFilesRequest, list_files};
use nerve_core::selection::{
    ManageSelectionMode, ManageSelectionOp, ManageSelectionRequest, SelectionKey, manage_selection,
};
use nerve_core::*;
use nerve_fs::{FsCatalogProvider, ScanOptions};
use std::fs;
use std::path::PathBuf;

// ---- read.rs ----

fn provider_for(root: &std::path::Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![root.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    )
}

#[test]
fn slices_lines_and_preserves_newlines() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.txt"),
            start_line: Some(2),
            end_line: Some(3),
            limit: None,
            snap: None,
        },
    )
    .expect("read");
    assert_eq!(response.content, "two\nthree\n");
}

#[test]
fn limit_wins_over_open_ended_slice() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.txt"),
            start_line: Some(2),
            end_line: None,
            limit: Some(1),
            snap: None,
        },
    )
    .expect("read");
    assert_eq!(response.first_line, 2);
    assert_eq!(response.last_line, 2);
    assert_eq!(response.content, "two\n");
}

#[test]
fn snap_none_matches_raw_output() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn a() {\n    let x = 1;\n}\n").expect("write");
    let provider = provider_for(dir.path());
    let raw = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.rs"),
            start_line: Some(2),
            end_line: None,
            limit: Some(1),
            snap: None,
        },
    )
    .expect("raw read");
    let none = read_file(
        &provider,
        &ReadFileRequest {
            snap: Some(ReadFileSnapMode::None),
            ..ReadFileRequest {
                path: dir.path().join("a.rs"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
                snap: None,
            }
        },
    )
    .expect("snap none read");
    assert_eq!(none.content, raw.content);
    assert_eq!(none.first_line, raw.first_line);
    assert_eq!(none.last_line, raw.last_line);
    assert_eq!(none.snap, None);
}

#[test]
fn snap_block_expands_from_opener_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn a() {\n    let x = 1;\n}\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.rs"),
            start_line: Some(1),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.first_line, 1);
    assert_eq!(response.last_line, 3);
    assert_eq!(response.content, "fn a() {\n    let x = 1;\n}\n");
    let snap = response.snap.expect("snap metadata");
    assert!(snap.applied);
    assert_eq!(snap.boundary_lines, vec![3]);
}

#[test]
fn snap_block_expands_from_interior_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn a() {\n    let x = 1;\n}\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.rs"),
            start_line: Some(2),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.first_line, 1);
    assert_eq!(response.last_line, 3);
    let snap = response.snap.expect("snap metadata");
    assert_eq!(snap.boundary_lines, vec![1, 3]);
}

#[test]
fn snap_block_expands_markdown_fenced_code_from_interior_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let markdown = concat!(
        "# Notes\n\n",
        "```rust\n",
        "pub fn fenced() {\n",
        "    println!(\"x\");\n",
        "}\n",
        "```\n",
        "tail\n"
    );
    fs::write(dir.path().join("README.md"), markdown).expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("README.md"),
            start_line: Some(5),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.first_line, 4);
    assert_eq!(response.last_line, 6);
    assert_eq!(
        response.content,
        "pub fn fenced() {\n    println!(\"x\");\n}\n"
    );
    let snap = response.snap.expect("snap metadata");
    assert!(snap.applied);
    assert_eq!(snap.boundary_lines, vec![4, 6]);
}

#[test]
fn snap_block_expands_indented_markdown_fenced_python() {
    let dir = tempfile::tempdir().expect("tempdir");
    let markdown = concat!(
        "   ```python\n",
        "   def fenced():\n",
        "       return 1\n",
        "   ```\n"
    );
    fs::write(dir.path().join("README.md"), markdown).expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("README.md"),
            start_line: Some(3),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.first_line, 2);
    assert_eq!(response.last_line, 3);
    assert_eq!(response.content, "   def fenced():\n       return 1\n");
    assert!(response.snap.expect("snap metadata").applied);
}

#[test]
fn snap_block_markdown_prose_still_falls_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("README.md"), "# Notes\n\nplain prose\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("README.md"),
            start_line: Some(3),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.content, "plain prose\n");
    assert_eq!(
        response.snap.expect("snap metadata").reason.as_deref(),
        Some("unsupported_language")
    );
}

#[test]
fn snap_block_unsupported_file_falls_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.txt"),
            start_line: Some(2),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.content, "two\n");
    assert_eq!(
        response.snap.expect("snap metadata").reason.as_deref(),
        Some("unsupported_language")
    );
}

#[test]
fn snap_block_syntax_error_falls_back() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn broken( {\n    let x = 1;\n}\n").expect("write");
    let provider = provider_for(dir.path());
    let response = read_file(
        &provider,
        &ReadFileRequest {
            path: dir.path().join("a.rs"),
            start_line: Some(2),
            end_line: None,
            limit: Some(1),
            snap: Some(ReadFileSnapMode::Block),
        },
    )
    .expect("read");
    assert_eq!(response.content, "    let x = 1;\n");
    assert_eq!(
        response.snap.expect("snap metadata").reason.as_deref(),
        Some("syntax_error")
    );
}

// ---- tree.rs ----

fn snapshot_for(dir: &std::path::Path) -> nerve_core::CatalogSnapshot {
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );
    provider.snapshot().expect("snapshot")
}

fn opts(mode: TreeMode, path: Option<&str>) -> FileTreeOptions {
    FileTreeOptions {
        mode,
        max_depth: None,
        path: path.map(str::to_string),
    }
}

#[test]
fn tree_mode_from_arg() {
    assert_eq!(TreeMode::from_arg(None), TreeMode::Auto);
    assert_eq!(TreeMode::from_arg(Some("auto")), TreeMode::Auto);
    assert_eq!(TreeMode::from_arg(Some("full")), TreeMode::Full);
    assert_eq!(TreeMode::from_arg(Some("folders")), TreeMode::Folders);
    assert_eq!(TreeMode::from_arg(Some("selected")), TreeMode::Auto);
    assert_eq!(TreeMode::from_arg(Some("bogus")), TreeMode::Auto);
}

#[test]
fn markers_show_selected_and_codemap_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("src")).expect("mkdir");
    fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
    fs::write(dir.path().join("src/b.py"), "def b():\n    pass\n").expect("write");
    fs::write(dir.path().join("README.md"), "```rust\nfn doc() {}\n```\n").expect("write");
    fs::write(dir.path().join("notes.txt"), "x\n").expect("write");
    let snap = snapshot_for(dir.path());
    let mut selection = Selection::default();
    for path in ["src/a.rs", "notes.txt"] {
        let entry = snap
            .entries
            .iter()
            .find(|entry| entry.rel_path == path)
            .expect("entry exists");
        selection.files.insert(
            SelectionKey {
                root_id: entry.root_id.clone(),
                path: entry.rel_path.clone(),
            },
            nerve_core::SelectionMode::Full,
        );
    }
    let resp = get_file_tree_with_selection(
        &snap,
        &FileTreeOptions {
            mode: TreeMode::Full,
            max_depth: None,
            path: None,
        },
        &selection,
    );

    assert!(resp.uses_legend);
    assert!(resp.tree.contains("a.rs *+"), "{}", resp.tree);
    assert!(resp.tree.contains("b.py +"), "{}", resp.tree);
    assert!(resp.tree.contains("README.md +"), "{}", resp.tree);
    assert!(resp.tree.contains("notes.txt *"), "{}", resp.tree);
}

#[test]
fn scoped_path_preserves_markers_for_original_rel_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("src")).expect("mkdir");
    fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
    fs::write(dir.path().join("src/readme.txt"), "x\n").expect("write");
    let snap = snapshot_for(dir.path());
    let entry = snap
        .entries
        .iter()
        .find(|entry| entry.rel_path == "src/a.rs")
        .expect("entry exists");
    let mut selection = Selection::default();
    selection.files.insert(
        SelectionKey {
            root_id: entry.root_id.clone(),
            path: entry.rel_path.clone(),
        },
        nerve_core::SelectionMode::Full,
    );

    let resp = get_file_tree_with_selection(
        &snap,
        &FileTreeOptions {
            mode: TreeMode::Full,
            max_depth: None,
            path: Some("src".to_string()),
        },
        &selection,
    );

    assert!(resp.tree.contains("a.rs *+"), "{}", resp.tree);
    assert!(resp.tree.contains("readme.txt"), "{}", resp.tree);
    assert!(resp.uses_legend);
}

#[test]
fn selected_mode_renders_only_selected_files_and_parents() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("src")).expect("mkdir");
    fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
    fs::write(dir.path().join("src/b.py"), "def b():\n    pass\n").expect("write");
    fs::write(dir.path().join("notes.txt"), "x\n").expect("write");
    let snap = snapshot_for(dir.path());
    let mut selection = Selection::default();
    for path in ["src/a.rs", "notes.txt"] {
        let entry = snap
            .entries
            .iter()
            .find(|entry| entry.rel_path == path)
            .expect("entry exists");
        selection.files.insert(
            SelectionKey {
                root_id: entry.root_id.clone(),
                path: entry.rel_path.clone(),
            },
            nerve_core::SelectionMode::Full,
        );
    }

    let resp =
        get_selected_file_tree_with_selection(&snap, &opts(TreeMode::Auto, None), &selection);

    assert!(resp.tree.contains("src/"), "{}", resp.tree);
    assert!(resp.tree.contains("a.rs *+"), "{}", resp.tree);
    assert!(resp.tree.contains("notes.txt *"), "{}", resp.tree);
    assert!(!resp.tree.contains("b.py"), "{}", resp.tree);
    assert!(resp.uses_legend);
}

#[test]
fn selected_mode_ignores_depth_to_keep_selected_files_visible() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("deep/a/b/c")).expect("mkdir");
    fs::write(dir.path().join("deep/a/b/c/file.rs"), "fn selected() {}\n").expect("write");
    let snap = snapshot_for(dir.path());
    let entry = snap
        .entries
        .iter()
        .find(|entry| entry.rel_path == "deep/a/b/c/file.rs")
        .expect("entry exists");
    let mut selection = Selection::default();
    selection.files.insert(
        SelectionKey {
            root_id: entry.root_id.clone(),
            path: entry.rel_path.clone(),
        },
        nerve_core::SelectionMode::Full,
    );
    let resp = get_selected_file_tree_with_selection(
        &snap,
        &FileTreeOptions {
            mode: TreeMode::Auto,
            max_depth: Some(1),
            path: None,
        },
        &selection,
    );

    assert!(resp.tree.contains("file.rs *+"), "{}", resp.tree);
    assert!(!resp.was_truncated);
}

#[test]
fn selected_mode_reports_empty_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").expect("write");
    let snap = snapshot_for(dir.path());
    let resp = get_selected_file_tree_with_selection(
        &snap,
        &opts(TreeMode::Auto, None),
        &Selection::default(),
    );

    assert!(resp.tree.is_empty());
    assert_eq!(resp.note.as_deref(), Some("selection is empty"));
}

#[test]
fn folders_mode_omits_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("src")).expect("mkdir");
    fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
    fs::write(dir.path().join("top.txt"), "x\n").expect("write");
    let snap = snapshot_for(dir.path());
    let resp = get_file_tree(&snap, &opts(TreeMode::Folders, None));
    assert!(resp.tree.contains("src"));
    assert!(!resp.tree.contains("a.rs"), "folders mode must omit files");
    assert!(!resp.tree.contains("top.txt"));
}

#[test]
fn path_scopes_to_subdirectory() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir(dir.path().join("src")).expect("mkdir");
    fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write");
    fs::write(dir.path().join("other.txt"), "x\n").expect("write");
    let snap = snapshot_for(dir.path());
    let resp = get_file_tree(&snap, &opts(TreeMode::Auto, Some("src")));
    assert!(resp.tree.contains("a.rs"));
    assert!(
        !resp.tree.contains("other.txt"),
        "path scope must exclude siblings"
    );
}

#[test]
fn unknown_path_reports_note() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "x\n").expect("write");
    let snap = snapshot_for(dir.path());
    let resp = get_file_tree(&snap, &opts(TreeMode::Auto, Some("does/not/exist")));
    assert!(resp.roots.is_empty());
    assert!(
        resp.note
            .as_deref()
            .unwrap_or_default()
            .contains("path not found")
    );
}

// ---- list_files.rs ----

#[test]
fn lists_files_with_selection_state_and_filter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(root.join("alpha.txt"), "a").expect("write alpha");
    fs::write(root.join("beta.rs"), "b").expect("write beta");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![root]).expect("root policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: vec![PathBuf::from("alpha.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select alpha");

    let all = list_files(&provider, &snapshot, &ListFilesRequest::default()).expect("list");
    assert_eq!(all.total, 2);
    assert!(!all.truncated);
    let alpha = all
        .files
        .iter()
        .find(|f| f.path == "alpha.txt")
        .expect("alpha");
    let beta = all
        .files
        .iter()
        .find(|f| f.path == "beta.rs")
        .expect("beta");
    assert!(alpha.selected, "the selected file is marked");
    assert!(!beta.selected, "an unselected file is not marked");

    // The query filters by path substring.
    let filtered = list_files(
        &provider,
        &snapshot,
        &ListFilesRequest {
            query: Some("beta".into()),
            limit: None,
        },
    )
    .expect("filtered");
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.files[0].path, "beta.rs");
}
