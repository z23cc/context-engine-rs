//! Relocated provider-dependent unit tests for `selection` and
//! `workspace_context`.
//!
//! These used to live in `nerve-core`'s in-src `#[cfg(test)]` modules but moved
//! out because they drive `nerve_fs::FsCatalogProvider` (the `dev-dependencies`
//! back-edge forbids constructing it in an in-src test — it would compile
//! `nerve-core` twice, "multiple versions of crate `nerve_core`"). They reach
//! only the public engine API, so no `test-internals` gate is needed.

use nerve_core::*;
use nerve_fs::{FsCatalogProvider, ScanOptions};
use serde_json::json;
use std::fs;
use std::path::PathBuf;

// ---- selection.rs ----

fn provider_with_files() -> FsCatalogProvider {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
    fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("write");
    let path = dir.keep();
    FsCatalogProvider::new(
        RootPolicy::new(vec![path]).expect("policy"),
        ScanOptions::default(),
    )
}

fn provider_with_reference_files() -> FsCatalogProvider {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("target.py"), "class Widget:\n    pass\n").expect("target");
    fs::write(
        dir.path().join("README.md"),
        "# Example\n\n```python\ndef example():\n    return Widget()\n```\n",
    )
    .expect("readme");
    fs::write(dir.path().join("note.txt"), "plain note\n").expect("note");
    let path = dir.keep();
    FsCatalogProvider::new(
        RootPolicy::new(vec![path]).expect("policy"),
        ScanOptions::default(),
    )
}

#[test]
fn selection_add_remove_and_mode_summary() {
    let provider = provider_with_files();
    let snapshot = provider.snapshot().expect("snapshot");

    let add = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: vec![PathBuf::from("a.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("add");
    assert_eq!(add.files.len(), 1);
    assert_eq!(add.files[0].mode, "full");
    assert!(add.files[0].token_estimate > 0);

    let set_slice = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: Vec::new(),
            mode: Some(ManageSelectionMode::Slices),
            slices: vec![SelectionSliceArg {
                path: PathBuf::from("a.txt"),
                ranges: vec![LineRange {
                    start_line: 2,
                    end_line: 2,
                    label: Some("middle line".to_string()),
                }],
            }],
            auto_codemap: false,
        },
    )
    .expect("set slice");
    assert_eq!(set_slice.files[0].mode, "slices");
    assert_eq!(set_slice.files[0].ranges[0].start_line, 2);
    assert_eq!(
        set_slice.files[0].ranges[0].label.as_deref(),
        Some("middle line")
    );
    assert!(set_slice.total_tokens < add.total_tokens);

    let removed = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Remove,
            paths: vec![PathBuf::from("a.txt")],
            mode: None,
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("remove");
    assert!(removed.files.is_empty());
}

#[test]
fn preview_summarizes_without_mutating_selection() {
    let provider = provider_with_files();
    let snapshot = provider.snapshot().expect("snapshot");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("a.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("initial selection");

    let preview = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Preview,
            paths: vec![PathBuf::from("lib.rs")],
            mode: Some(ManageSelectionMode::CodemapOnly),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("preview");
    assert!(preview.preview);
    assert!(preview.would_mutate);
    assert!(!preview.mutated);
    assert_eq!(preview.files.len(), 2);
    assert!(
        preview
            .files
            .iter()
            .any(|file| file.path == "a.txt" && file.mode == "full")
    );
    assert!(
        preview
            .files
            .iter()
            .any(|file| file.path == "lib.rs" && file.mode == "codemap_only")
    );

    let persisted = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Get,
            paths: Vec::new(),
            mode: None,
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("get");
    assert_eq!(persisted.files.len(), 1);
    assert_eq!(persisted.files[0].path, "a.txt");
}

#[test]
fn promote_and_demote_convert_selected_modes() {
    let provider = provider_with_files();
    let snapshot = provider.snapshot().expect("snapshot");
    let selected = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("lib.rs")],
            mode: Some(ManageSelectionMode::CodemapOnly),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("codemap selection");
    assert_eq!(selected.files[0].mode, "codemap_only");

    let promoted = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Promote,
            paths: vec![PathBuf::from("lib.rs")],
            mode: None,
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("promote");
    assert!(promoted.mutated);
    assert_eq!(promoted.files[0].mode, "full");

    let demoted = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Demote,
            paths: Vec::new(),
            mode: None,
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("demote all");
    assert!(demoted.mutated);
    assert_eq!(demoted.files[0].mode, "codemap_only");
}

#[test]
fn root_prefixed_paths_disambiguate_multi_root_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    fs::create_dir_all(&left).expect("left dir");
    fs::create_dir_all(&right).expect("right dir");
    fs::write(left.join("common.txt"), "left\n").expect("left file");
    fs::write(right.join("common.txt"), "right\n").expect("right file");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![left, right]).expect("policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");

    let empty = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("empty path selects nothing");
    assert!(empty.files.is_empty());

    let both = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("common.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select both");
    assert_eq!(both.files.len(), 2);

    let right_only = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("right/common.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select by root name");
    assert_eq!(right_only.files.len(), 1);
    assert_eq!(right_only.files[0].root_id, "root-1");
    assert!(
        right_only.files[0]
            .display_path
            .ends_with("right/common.txt")
    );

    let left_by_id = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("root-0/common.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select by root id");
    assert_eq!(left_by_id.files.len(), 1);
    assert_eq!(left_by_id.files[0].root_id, "root-0");
}

#[test]
fn auto_codemap_adds_referenced_definition_files_when_requested() {
    let provider = provider_with_reference_files();
    let snapshot = provider.snapshot().expect("snapshot");

    let summary = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("README.md")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: true,
        },
    )
    .expect("auto codemap selection");

    assert!(summary.mutated);
    assert_eq!(summary.auto_codemap_added, 1);
    assert_eq!(summary.files.len(), 2);
    assert!(
        summary
            .files
            .iter()
            .any(|file| file.path == "README.md" && file.mode == "full")
    );
    assert!(
        summary
            .files
            .iter()
            .any(|file| file.path == "target.py" && file.mode == "codemap_only")
    );

    let persisted = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Get,
            paths: Vec::new(),
            mode: None,
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("persisted selection");
    assert_eq!(persisted.files.len(), 2);
    assert!(
        persisted
            .files
            .iter()
            .any(|file| file.path == "target.py" && file.mode == "codemap_only")
    );
}

#[test]
fn auto_codemap_is_explicit_and_skips_codemap_only_requests() {
    let provider = provider_with_reference_files();
    let snapshot = provider.snapshot().expect("snapshot");

    let manual = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("README.md")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("manual selection");
    assert_eq!(manual.auto_codemap_added, 0);
    assert_eq!(manual.files.len(), 1);
    assert_eq!(manual.files[0].path, "README.md");

    let codemap_only = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("README.md")],
            mode: Some(ManageSelectionMode::CodemapOnly),
            slices: Vec::new(),
            auto_codemap: true,
        },
    )
    .expect("codemap-only selection");
    assert_eq!(codemap_only.auto_codemap_added, 0);
    assert_eq!(codemap_only.files.len(), 1);
    assert_eq!(codemap_only.files[0].path, "README.md");
    assert_eq!(codemap_only.files[0].mode, "codemap_only");

    let add_note = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: vec![PathBuf::from("note.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: true,
        },
    )
    .expect("add note with auto codemap");
    assert_eq!(add_note.auto_codemap_added, 0);

    let persisted = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Get,
            paths: Vec::new(),
            mode: None,
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("persisted selection");
    assert!(
        !persisted.files.iter().any(|file| file.path == "target.py"),
        "pre-existing codemap_only README.md must not seed auto expansion"
    );
}

#[test]
fn auto_codemap_keeps_multi_root_reference_seeds_isolated() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    fs::create_dir_all(&left).expect("left dir");
    fs::create_dir_all(&right).expect("right dir");
    fs::write(
        left.join("README.md"),
        "# Left\n\n```python\ndef left():\n    return Widget()\n```\n",
    )
    .expect("left readme");
    fs::write(left.join("target.py"), "class Widget:\n    pass\n").expect("left target");
    fs::write(
        right.join("README.md"),
        "# Right\n\n```python\ndef right():\n    return Gadget()\n```\n",
    )
    .expect("right readme");
    fs::write(right.join("gadget.py"), "class Gadget:\n    pass\n").expect("right gadget");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![left, right]).expect("policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");

    let summary = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("root-0/README.md")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: true,
        },
    )
    .expect("multi-root auto codemap");

    assert_eq!(summary.auto_codemap_added, 1);
    assert!(summary.files.iter().any(|file| {
        file.root_id == "root-0" && file.path == "target.py" && file.mode == "codemap_only"
    }));
    assert!(
        !summary
            .files
            .iter()
            .any(|file| file.root_id == "root-1" || file.path == "gadget.py"),
        "auto expansion must not read references or definitions from unseeded roots"
    );
}

#[test]
fn line_range_label_json_compatibility_and_aliases() {
    let plain: LineRange = serde_json::from_value(json!({
        "start_line": 1,
        "end_line": 2
    }))
    .expect("plain line range");
    assert_eq!(plain, LineRange::new(1, 2));

    let described: LineRange = serde_json::from_value(json!({
        "start_line": 3,
        "end_line": 4,
        "description": "why"
    }))
    .expect("description alias");
    assert_eq!(described, LineRange::with_label(3, 4, "why"));

    let desc: LineRange = serde_json::from_value(json!({
        "start_line": 5,
        "end_line": 6,
        "desc": "short"
    }))
    .expect("desc alias");
    assert_eq!(desc, LineRange::with_label(5, 6, "short"));

    let duplicate = serde_json::from_value::<LineRange>(json!({
        "start_line": 1,
        "end_line": 1,
        "label": "a",
        "description": "b"
    }));
    assert!(duplicate.is_err(), "duplicate label aliases are rejected");
}

#[test]
fn codemap_only_counts_codemap_tokens() {
    let provider = provider_with_files();
    let snapshot = provider.snapshot().expect("snapshot");
    let summary = manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("lib.rs")],
            mode: Some(ManageSelectionMode::CodemapOnly),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("codemap selection");
    assert_eq!(summary.files[0].mode, "codemap_only");
    assert!(summary.files[0].token_estimate > 0);
}

// ---- workspace_context.rs ----

fn provider_with_selection() -> (FsCatalogProvider, CatalogSnapshot) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("full.txt"), "full file\n").expect("write");
    fs::write(dir.path().join("notes.txt"), "one\ntwo\nthree\n").expect("write");
    fs::write(dir.path().join("lib.rs"), "pub fn alpha() {}\n").expect("write");
    let path = dir.keep();
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![path]).expect("policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![PathBuf::from("full.txt")],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select full");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: Vec::new(),
            mode: Some(ManageSelectionMode::Slices),
            slices: vec![SelectionSliceArg {
                path: PathBuf::from("notes.txt"),
                ranges: vec![LineRange {
                    start_line: 2,
                    end_line: 2,
                    label: Some("key & \"<finding>\nnext".to_string()),
                }],
            }],
            auto_codemap: false,
        },
    )
    .expect("replace with slice");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Add,
            paths: vec![PathBuf::from("lib.rs")],
            mode: Some(ManageSelectionMode::CodemapOnly),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select codemap");
    (provider, snapshot)
}

#[test]
fn renders_modes_and_token_breakdown() {
    let (provider, snapshot) = provider_with_selection();
    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            include: Vec::new(),
            instructions: Some("Use this context.".to_string()),
            ..Default::default()
        },
    )
    .expect("workspace context");

    assert!(response.context.contains("<file_map>"));
    assert!(response.context.contains("<instructions>"));
    assert!(response.context.contains("mode=\"full\""));
    assert!(response.context.contains("mode=\"slices\""));
    assert!(
        response
            .context
            .contains("description=\"key &amp; &quot;&lt;finding&gt; next\"")
    );
    assert!(response.tokens.files.iter().any(|file| {
        file.mode == "slices"
            && file
                .segments
                .iter()
                .any(|segment| segment.label == "key & \"<finding> next")
    }));
    assert!(response.context.contains("mode=\"codemap_only\""));
    assert!(response.context.contains("- function alpha @ line 1"));
    assert_eq!(response.context_hash.len(), 32);
    assert_eq!(response.tokens.files.len(), 3);
    assert!(
        response
            .tokens
            .files
            .iter()
            .all(|file| file.content_hash.len() == 32)
    );
    assert!(response.tokens.total_tokens > 0);
    assert!(
        response
            .tokens
            .files
            .iter()
            .any(|file| file.mode == "slices" && !file.segments.is_empty())
    );
}

#[test]
fn content_hash_is_stable_and_changes_with_rendered_context() {
    let (provider, snapshot) = provider_with_selection();
    let request = WorkspaceContextRequest {
        include: vec![
            WorkspaceContextInclude::FileMap,
            WorkspaceContextInclude::Contents,
        ],
        instructions: None,
        ..Default::default()
    };
    let first = workspace_context(&provider, &snapshot, &request).expect("first context");
    let second = workspace_context(&provider, &snapshot, &request).expect("second context");
    assert_eq!(first.context_hash, second.context_hash);
    assert_eq!(
        first.tokens.files[0].content_hash,
        second.tokens.files[0].content_hash
    );

    let file_map_only = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            include: vec![WorkspaceContextInclude::FileMap],
            instructions: None,
            ..Default::default()
        },
    )
    .expect("file map context");
    assert_ne!(first.context_hash, file_map_only.context_hash);
}

#[test]
fn include_can_omit_contents_from_context_text() {
    let (provider, snapshot) = provider_with_selection();
    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            include: vec![WorkspaceContextInclude::FileMap],
            instructions: None,
            ..Default::default()
        },
    )
    .expect("workspace context");

    assert!(response.context.contains("<file_map>"));
    assert!(!response.context.contains("<file path="));
    assert_eq!(response.tokens.contents_tokens, 0);
}

#[test]
fn include_tree_and_code_sections_for_selected_files() {
    let (provider, snapshot) = provider_with_selection();
    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            include: vec![WorkspaceContextInclude::Tree, WorkspaceContextInclude::Code],
            instructions: None,
            ..Default::default()
        },
    )
    .expect("workspace context");

    assert!(response.context.contains("<file_tree>"));
    assert!(
        response
            .context
            .contains("legend: * selected, + codemap-capable")
    );
    assert!(response.context.contains("lib.rs *+"));
    assert!(response.context.contains("<code_structure>"));
    assert!(response.context.contains("lib.rs"));
    assert!(response.context.contains("function (1): pub fn alpha()"));
    assert!(!response.context.contains("<file path="));
    assert_eq!(response.tokens.contents_tokens, 0);
    assert!(response.tokens.tree_tokens > 0);
    assert!(response.tokens.code_tokens > 0);
}

#[test]
fn code_section_disambiguates_duplicate_relative_paths() {
    let dir = tempfile::tempdir().expect("tempdir");
    let left = dir.path().join("left");
    let right = dir.path().join("right");
    fs::create_dir_all(&left).expect("left dir");
    fs::create_dir_all(&right).expect("right dir");
    fs::write(left.join("common.rs"), "pub fn left_api() {}\n").expect("left file");
    fs::write(right.join("common.rs"), "pub fn right_api() {}\n").expect("right file");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![left, right]).expect("policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");
    manage_selection(
        &provider,
        &snapshot,
        &ManageSelectionRequest {
            op: ManageSelectionOp::Set,
            paths: vec![
                PathBuf::from("root-0/common.rs"),
                PathBuf::from("root-1/common.rs"),
            ],
            mode: Some(ManageSelectionMode::Full),
            slices: Vec::new(),
            auto_codemap: false,
        },
    )
    .expect("select both roots");

    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            include: vec![WorkspaceContextInclude::Code],
            ..Default::default()
        },
    )
    .expect("workspace context");

    assert!(response.context.contains("left/common.rs"));
    assert!(response.context.contains("right/common.rs"));
    assert!(response.context.contains("left_api"));
    assert!(response.context.contains("right_api"));
}

#[test]
fn recipe_review_assembles_git_diff_and_default_meta_prompt() {
    let (provider, snapshot) = provider_with_selection();
    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            recipe: Some("review".to_string()),
            git_diff: Some("diff --git a/full.txt b/full.txt\n+added line".to_string()),
            ..Default::default()
        },
    )
    .expect("workspace context");

    // review = file_map + contents + git_diff + meta_prompts (default Review).
    assert!(response.context.contains("<file_map>"));
    assert!(response.context.contains("<git_diff>"));
    assert!(response.context.contains("diff --git"));
    assert!(response.context.contains("<meta prompt 1=\"Review\">"));
    assert!(response.tokens.git_diff_tokens > 0);
    assert!(response.tokens.meta_prompts_tokens > 0);
}

#[test]
fn recipe_diff_is_git_only() {
    let (provider, snapshot) = provider_with_selection();
    let response = workspace_context(
        &provider,
        &snapshot,
        &WorkspaceContextRequest {
            recipe: Some("diff".to_string()),
            git_diff: Some("diff --git a/x b/x\n+y".to_string()),
            ..Default::default()
        },
    )
    .expect("workspace context");

    assert!(response.context.contains("<git_diff>"));
    assert!(!response.context.contains("<file_map>"));
    assert!(!response.context.contains("<meta prompt"));
    assert_eq!(response.tokens.file_map_tokens, 0);
    assert_eq!(response.tokens.contents_tokens, 0);
}
