//! Compact file tree rendering from a snapshot, with an auto-budget mode.
//!
//! `auto` mode (default) targets a character budget and degrades the view
//! (depth -> directories-only -> top-level) so large repositories stay within
//! client token limits, attaching a `note` that explains any degradation.
//! `full` and `folders` render unbounded — explicit escape hatches.
//! `selected` renders only the current selection and its parent directories.

use crate::{
    models::*,
    selection::{Selection, SelectionKey},
    snapshot::CatalogSnapshot,
};
use std::collections::{BTreeMap, BTreeSet};

/// Character budget for `auto` mode (~4k tokens; comfortably under client caps).
const AUTO_BUDGET_CHARS: usize = 16_000;
/// Maximum children rendered per directory in `auto` mode.
const AUTO_SIBLING_CAP: usize = 100;
/// Sentinel meaning "no depth limit".
const UNLIMITED_DEPTH: usize = usize::MAX;

/// Tree filter mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    /// Degrade depth/breadth to fit a character budget (default).
    Auto,
    /// Full tree, unbounded (can be very large).
    Full,
    /// Directories only, unbounded.
    Folders,
}

impl TreeMode {
    #[must_use]
    pub fn from_arg(value: Option<&str>) -> Self {
        match value.unwrap_or("auto") {
            "full" => Self::Full,
            "folders" => Self::Folders,
            _ => Self::Auto,
        }
    }
}

/// Options for [`get_file_tree`].
#[derive(Debug, Clone)]
pub struct FileTreeOptions {
    pub mode: TreeMode,
    pub max_depth: Option<usize>,
    /// Optional rel-path prefix scoping the tree to a subdirectory.
    pub path: Option<String>,
}

#[derive(Default)]
struct NodeBuilder {
    files: BTreeMap<String, FileNodeData>,
    dirs: BTreeMap<String, NodeBuilder>,
}

#[derive(Debug, Clone)]
struct FileNodeData {
    path: String,
    rel_path: String,
    root_id: String,
}

struct MaterializeContext<'a> {
    max_depth: usize,
    sibling_cap: Option<usize>,
    skip_noise: bool,
    folders_only: bool,
    selection: &'a Selection,
}

/// Directories pruned in `auto` mode (build artifacts / VCS / caches).
fn noise_dir(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | ".git"
            | ".hg"
            | ".svn"
            | "target"
            | "dist"
            | "build"
            | "out"
            | "coverage"
            | "vendor"
            | ".next"
            | ".nuxt"
            | ".turbo"
            | ".cache"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".idea"
            | ".gradle"
            | "DerivedData"
            | "Pods"
    )
}

/// Build a compact file tree from snapshot paths.
#[must_use]
pub fn get_file_tree(snapshot: &CatalogSnapshot, options: &FileTreeOptions) -> FileTreeResponse {
    let selection = Selection::default();
    get_file_tree_with_selection(snapshot, options, &selection)
}

/// Build a compact file tree, marking files from the current selection.
#[must_use]
pub fn get_file_tree_with_selection(
    snapshot: &CatalogSnapshot,
    options: &FileTreeOptions,
    selection: &Selection,
) -> FileTreeResponse {
    let prefix = options.path.as_deref().map(normalize_prefix);

    let response = match options.mode {
        TreeMode::Full => {
            let (roots, omitted) = build(
                snapshot,
                prefix.as_deref(),
                options.max_depth.unwrap_or(UNLIMITED_DEPTH),
                None,
                false,
                false,
                selection,
            );
            finalize(roots, omitted, false, None)
        }
        TreeMode::Folders => {
            let (roots, omitted) = build(
                snapshot,
                prefix.as_deref(),
                options.max_depth.unwrap_or(UNLIMITED_DEPTH),
                None,
                false,
                true,
                selection,
            );
            finalize(roots, omitted, false, None)
        }
        TreeMode::Auto => build_auto(snapshot, prefix.as_deref(), options.max_depth, selection),
    };

    apply_missing_path_note(snapshot, options, prefix.as_deref(), response)
}

/// Build a selected-only tree using the current selection.
#[must_use]
pub fn get_selected_file_tree_with_selection(
    snapshot: &CatalogSnapshot,
    options: &FileTreeOptions,
    selection: &Selection,
) -> FileTreeResponse {
    let prefix = options.path.as_deref().map(normalize_prefix);
    let response = build_selected(snapshot, prefix.as_deref(), selection);
    apply_missing_path_note(snapshot, options, prefix.as_deref(), response)
}

fn apply_missing_path_note(
    snapshot: &CatalogSnapshot,
    options: &FileTreeOptions,
    prefix: Option<&str>,
    response: FileTreeResponse,
) -> FileTreeResponse {
    if prefix.is_some_and(|p| !p.is_empty() && !prefix_matches_snapshot(snapshot, p)) {
        return FileTreeResponse {
            note: Some(format!(
                "path not found in catalog: {}",
                options.path.as_deref().unwrap_or_default()
            )),
            ..response
        };
    }
    response
}

fn normalize_prefix(path: &str) -> String {
    path.trim().trim_matches('/').to_string()
}

/// `auto`: try progressively coarser views, accept the first that fits the budget.
fn build_auto(
    snapshot: &CatalogSnapshot,
    prefix: Option<&str>,
    caller_depth: Option<usize>,
    selection: &Selection,
) -> FileTreeResponse {
    let first_depth = caller_depth.unwrap_or(UNLIMITED_DEPTH);
    // (folders_only, depth, degradation note)
    let attempts: [(bool, usize, Option<&'static str>); 5] = [
        (false, first_depth, None),
        (false, 3, Some("depth capped at 3")),
        (true, first_depth, Some("directories only")),
        (true, 3, Some("directories only, depth capped at 3")),
        (
            true,
            1,
            Some("directories only, top level — pass `path` to scope into a subdirectory"),
        ),
    ];

    let mut fallback: Option<(Vec<FileTreeNode>, usize, String)> = None;
    for (folders_only, depth, note) in attempts {
        let (roots, omitted) = build(
            snapshot,
            prefix,
            depth,
            Some(AUTO_SIBLING_CAP),
            true,
            folders_only,
            selection,
        );
        let tree = render_ascii_tree(&roots);
        if tree.len() <= AUTO_BUDGET_CHARS {
            let final_note = note.map(str::to_string).or_else(|| {
                (omitted > 0).then(|| {
                    format!(
                        "{omitted} entries omitted to fit the size budget; pass `path` to scope in"
                    )
                })
            });
            return finalize(roots, omitted, false, final_note);
        }
        fallback = Some((roots, omitted, note.unwrap_or("large tree").to_string()));
    }

    // Even the coarsest view is over budget: hard-truncate as a last resort.
    let (roots, omitted, _) = fallback.expect("attempts is non-empty");
    let tree = truncate_to_chars(&render_ascii_tree(&roots), AUTO_BUDGET_CHARS);
    FileTreeResponse {
        roots_count: roots.len(),
        was_truncated: true,
        uses_legend: roots.iter().any(node_uses_legend),
        roots,
        tree,
        omitted,
        note: Some(
            "tree exceeds the size budget; output truncated — pass `path` to scope to a subdirectory"
                .to_string(),
        ),
    }
}

fn build_selected(
    snapshot: &CatalogSnapshot,
    prefix: Option<&str>,
    selection: &Selection,
) -> FileTreeResponse {
    let selected = selected_keys(selection);
    let (roots, omitted) =
        build_selected_roots(snapshot, prefix, UNLIMITED_DEPTH, selection, &selected);
    let note = selected_note(selection, prefix, roots.is_empty());
    finalize(roots, omitted, false, note)
}

fn selected_keys(selection: &Selection) -> BTreeSet<SelectionKey> {
    selection.files.keys().cloned().collect()
}

fn build_selected_roots(
    snapshot: &CatalogSnapshot,
    prefix: Option<&str>,
    max_depth: usize,
    selection: &Selection,
    selected: &BTreeSet<SelectionKey>,
) -> (Vec<FileTreeNode>, usize) {
    let mut roots = Vec::new();
    let mut omitted = 0usize;
    for root in &snapshot.roots {
        let mut builder = NodeBuilder::default();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| entry.root_id == root.id)
        {
            if !selected.contains(&selection_key_for_entry(entry)) {
                continue;
            }
            let Some(rel) = scoped_rel(&entry.rel_path, prefix) else {
                continue;
            };
            insert(&mut builder, rel, file_node_data(entry, rel));
        }
        if builder_is_empty(&builder) {
            continue;
        }
        let context = MaterializeContext {
            max_depth,
            sibling_cap: None,
            skip_noise: false,
            folders_only: false,
            selection,
        };
        let children = materialize(builder, 0, &context, &mut omitted);
        roots.push(root_node(root_name(root), prefix, children));
    }
    (roots, omitted)
}

fn selected_note(selection: &Selection, prefix: Option<&str>, roots_empty: bool) -> Option<String> {
    if !roots_empty {
        return None;
    }
    if selection.files.is_empty() {
        return Some("selection is empty".to_string());
    }
    Some(match prefix.filter(|p| !p.is_empty()) {
        Some(path) => format!("no selected files under path: {path}"),
        None => "selection does not match the current catalog".to_string(),
    })
}

fn prefix_matches_snapshot(snapshot: &CatalogSnapshot, prefix: &str) -> bool {
    prefix.is_empty()
        || snapshot
            .entries
            .iter()
            .any(|entry| scoped_rel(&entry.rel_path, Some(prefix)).is_some())
}

fn finalize(
    roots: Vec<FileTreeNode>,
    omitted: usize,
    forced_truncated: bool,
    note: Option<String>,
) -> FileTreeResponse {
    let uses_legend = roots.iter().any(node_uses_legend);
    let tree = render_ascii_tree(&roots);
    FileTreeResponse {
        roots_count: roots.len(),
        was_truncated: forced_truncated || omitted > 0,
        uses_legend,
        roots,
        tree,
        omitted,
        note,
    }
}

#[allow(clippy::fn_params_excessive_bools)]
fn build(
    snapshot: &CatalogSnapshot,
    prefix: Option<&str>,
    max_depth: usize,
    sibling_cap: Option<usize>,
    skip_noise: bool,
    folders_only: bool,
    selection: &Selection,
) -> (Vec<FileTreeNode>, usize) {
    let mut roots = Vec::new();
    let mut omitted = 0usize;

    for root in &snapshot.roots {
        let mut builder = NodeBuilder::default();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| entry.root_id == root.id)
        {
            let Some(rel) = scoped_rel(&entry.rel_path, prefix) else {
                continue;
            };
            insert(&mut builder, rel, file_node_data(entry, rel));
        }
        if builder_is_empty(&builder) {
            continue;
        }

        let context = MaterializeContext {
            max_depth,
            sibling_cap,
            skip_noise,
            folders_only,
            selection,
        };
        let children = materialize(builder, 0, &context, &mut omitted);

        roots.push(root_node(root_name(root), prefix, children));
    }
    (roots, omitted)
}

/// Return the portion of `rel` under directory `prefix`, or `None` if not under it.
fn strip_under<'a>(rel: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        return Some(rel);
    }
    let rest = rel.strip_prefix(prefix)?;
    if rest.is_empty() {
        return Some("");
    }
    rest.strip_prefix('/')
}

fn scoped_rel<'a>(rel_path: &'a str, prefix: Option<&str>) -> Option<&'a str> {
    match prefix {
        Some(prefix) => strip_under(rel_path, prefix).filter(|suffix| !suffix.is_empty()),
        None => Some(rel_path),
    }
}

fn file_node_data(entry: &CatalogEntry, path: &str) -> FileNodeData {
    FileNodeData {
        path: path.to_string(),
        rel_path: entry.rel_path.clone(),
        root_id: entry.root_id.clone(),
    }
}

fn selection_key_for_entry(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

fn root_name(root: &RootRef) -> String {
    root.path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

fn root_node(base: String, prefix: Option<&str>, children: Vec<FileTreeNode>) -> FileTreeNode {
    let name = match prefix {
        Some(prefix) if !prefix.is_empty() => format!("{base}/{prefix}"),
        _ => base,
    };
    FileTreeNode {
        name,
        path: prefix.unwrap_or_default().to_string(),
        kind: FileTreeKind::Directory,
        markers: Vec::new(),
        children,
    }
}

fn builder_is_empty(builder: &NodeBuilder) -> bool {
    builder.files.is_empty() && builder.dirs.is_empty()
}

fn insert(builder: &mut NodeBuilder, rel_path: &str, file: FileNodeData) {
    let mut parts = rel_path.split('/').filter(|part| !part.is_empty());
    if let Some(first) = parts.next() {
        let rest: Vec<_> = parts.collect();
        if rest.is_empty() {
            builder.files.insert(first.to_string(), file);
        } else {
            insert(
                builder.dirs.entry(first.to_string()).or_default(),
                &rest.join("/"),
                file,
            );
        }
    }
}

fn materialize(
    builder: NodeBuilder,
    depth: usize,
    context: &MaterializeContext<'_>,
    omitted: &mut usize,
) -> Vec<FileTreeNode> {
    let mut nodes = Vec::new();
    let mut emitted = 0usize;
    let cap = context.sibling_cap.unwrap_or(usize::MAX);

    for (name, child) in builder.dirs {
        if context.skip_noise && noise_dir(&name) {
            continue; // pruned by policy; not counted as omitted
        }
        if depth >= context.max_depth || emitted >= cap {
            *omitted += 1 + count_builder(&child);
            continue;
        }
        emitted += 1;
        nodes.push(FileTreeNode {
            path: name.clone(),
            name,
            kind: FileTreeKind::Directory,
            markers: Vec::new(),
            children: materialize(child, depth + 1, context, omitted),
        });
    }

    if context.folders_only {
        return nodes;
    }
    for (name, file) in builder.files {
        if emitted >= cap {
            *omitted += 1;
            continue;
        }
        emitted += 1;
        nodes.push(FileTreeNode {
            name,
            path: file.path.clone(),
            kind: FileTreeKind::File,
            markers: file_markers(&file, context.selection),
            children: Vec::new(),
        });
    }
    nodes
}

fn count_builder(builder: &NodeBuilder) -> usize {
    builder.files.len()
        + builder.dirs.len()
        + builder.dirs.values().map(count_builder).sum::<usize>()
}

fn file_markers(file: &FileNodeData, selection: &Selection) -> Vec<FileTreeMarker> {
    let key = SelectionKey {
        root_id: file.root_id.clone(),
        path: file.rel_path.clone(),
    };
    let mut markers = Vec::new();
    if selection.files.contains_key(&key) {
        markers.push(FileTreeMarker::Selected);
    }
    if crate::codemap::path_supports_codemap(&file.rel_path) {
        markers.push(FileTreeMarker::Codemap);
    }
    markers
}

fn node_uses_legend(node: &FileTreeNode) -> bool {
    !node.markers.is_empty() || node.children.iter().any(node_uses_legend)
}

fn render_node_name(node: &FileTreeNode) -> String {
    let marker_text = marker_text(&node.markers);
    if marker_text.is_empty() {
        return node.name.clone();
    }
    format!("{} {marker_text}", node.name)
}

fn marker_text(markers: &[FileTreeMarker]) -> String {
    markers.iter().map(marker_symbol).collect()
}

fn marker_symbol(marker: &FileTreeMarker) -> char {
    match marker {
        FileTreeMarker::Selected => '*',
        FileTreeMarker::Codemap => '+',
    }
}

fn truncate_to_chars(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let cut = text[..end].rfind('\n').unwrap_or(end);
    format!("{}\n…", &text[..cut])
}

fn render_ascii_tree(roots: &[FileTreeNode]) -> String {
    let mut lines = Vec::new();
    for root in roots {
        lines.push(render_node_name(root));
        render_children(&root.children, String::new(), &mut lines);
    }
    lines.join("\n")
}

fn render_children(children: &[FileTreeNode], prefix: String, lines: &mut Vec<String>) {
    for (idx, child) in children.iter().enumerate() {
        let is_last = idx + 1 == children.len();
        let connector = if is_last { "└── " } else { "├── " };
        let suffix = if child.kind == FileTreeKind::Directory {
            "/"
        } else {
            ""
        };
        lines.push(format!(
            "{prefix}{connector}{}{suffix}",
            render_node_name(child)
        ));
        if !child.children.is_empty() {
            let child_prefix = if is_last { "    " } else { "│   " };
            render_children(&child.children, format!("{prefix}{child_prefix}"), lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, catalog::ScanOptions, port::CatalogProvider};
    use std::fs;

    fn snapshot_for(dir: &std::path::Path) -> crate::CatalogSnapshot {
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
                crate::SelectionMode::Full,
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
            crate::SelectionMode::Full,
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
                crate::SelectionMode::Full,
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
            crate::SelectionMode::Full,
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

    #[test]
    fn auto_sibling_cap_truncates_wide_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        for i in 0..1500 {
            fs::write(dir.path().join(format!("f{i:04}.txt")), "x\n").expect("write");
        }
        let snap = snapshot_for(dir.path());
        let resp = get_file_tree(&snap, &opts(TreeMode::Auto, None));
        let lines = resp.tree.lines().count();
        assert!(
            lines <= AUTO_SIBLING_CAP + 5,
            "wide dir should be capped, got {lines} lines"
        );
        assert!(resp.was_truncated);
        assert!(resp.omitted >= 1500 - AUTO_SIBLING_CAP);
        assert!(resp.note.is_some());
    }
}
