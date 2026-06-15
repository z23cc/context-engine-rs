//! Compact file tree rendering from a snapshot, with an auto-budget mode.
//!
//! `auto` mode (default) targets a character budget and degrades the view
//! (depth -> directories-only -> top-level) so large repositories stay within
//! client token limits, attaching a `note` that explains any degradation.
//! `full` and `folders` render unbounded — explicit escape hatches.

use crate::{models::*, snapshot::CatalogSnapshot};
use std::collections::BTreeMap;

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
    files: BTreeMap<String, String>,
    dirs: BTreeMap<String, NodeBuilder>,
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
            );
            finalize(roots, omitted, false, None)
        }
        TreeMode::Auto => build_auto(snapshot, prefix.as_deref(), options.max_depth),
    };

    if prefix.as_deref().is_some_and(|p| !p.is_empty()) && response.roots.is_empty() {
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
        uses_legend: false,
        roots,
        tree,
        omitted,
        note: Some(
            "tree exceeds the size budget; output truncated — pass `path` to scope to a subdirectory"
                .to_string(),
        ),
    }
}

fn finalize(
    roots: Vec<FileTreeNode>,
    omitted: usize,
    forced_truncated: bool,
    note: Option<String>,
) -> FileTreeResponse {
    let tree = render_ascii_tree(&roots);
    FileTreeResponse {
        roots_count: roots.len(),
        was_truncated: forced_truncated || omitted > 0,
        uses_legend: false,
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
) -> (Vec<FileTreeNode>, usize) {
    let mut roots = Vec::new();
    let mut omitted = 0usize;

    for root in &snapshot.roots {
        let mut builder = NodeBuilder::default();
        let mut matched = false;
        for entry in snapshot.entries.iter().filter(|e| e.root_id == root.id) {
            let rel = match prefix {
                Some(p) => match strip_under(&entry.rel_path, p) {
                    Some(suffix) if !suffix.is_empty() => suffix,
                    _ => continue,
                },
                None => entry.rel_path.as_str(),
            };
            insert(&mut builder, rel, rel);
            matched = true;
        }
        if prefix.is_some_and(|p| !p.is_empty()) && !matched {
            continue;
        }

        let children = materialize(
            builder,
            0,
            max_depth,
            sibling_cap,
            skip_noise,
            folders_only,
            &mut omitted,
        );

        let base = root
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let name = match prefix {
            Some(p) if !p.is_empty() => format!("{base}/{p}"),
            _ => base,
        };
        roots.push(FileTreeNode {
            name,
            path: prefix.unwrap_or_default().to_string(),
            kind: FileTreeKind::Directory,
            children,
        });
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

fn insert(builder: &mut NodeBuilder, rel_path: &str, full: &str) {
    let mut parts = rel_path.split('/').filter(|part| !part.is_empty());
    if let Some(first) = parts.next() {
        let rest: Vec<_> = parts.collect();
        if rest.is_empty() {
            builder.files.insert(first.to_string(), full.to_string());
        } else {
            insert(
                builder.dirs.entry(first.to_string()).or_default(),
                &rest.join("/"),
                full,
            );
        }
    }
}

#[allow(clippy::fn_params_excessive_bools)]
fn materialize(
    builder: NodeBuilder,
    depth: usize,
    max_depth: usize,
    sibling_cap: Option<usize>,
    skip_noise: bool,
    folders_only: bool,
    omitted: &mut usize,
) -> Vec<FileTreeNode> {
    let mut nodes = Vec::new();
    let mut emitted = 0usize;
    let cap = sibling_cap.unwrap_or(usize::MAX);

    for (name, child) in builder.dirs {
        if skip_noise && noise_dir(&name) {
            continue; // pruned by policy; not counted as omitted
        }
        if depth >= max_depth || emitted >= cap {
            *omitted += 1 + count_builder(&child);
            continue;
        }
        emitted += 1;
        nodes.push(FileTreeNode {
            path: name.clone(),
            name,
            kind: FileTreeKind::Directory,
            children: materialize(
                child,
                depth + 1,
                max_depth,
                sibling_cap,
                skip_noise,
                folders_only,
                omitted,
            ),
        });
    }

    if folders_only {
        return nodes;
    }
    for (name, path) in builder.files {
        if emitted >= cap {
            *omitted += 1;
            continue;
        }
        emitted += 1;
        nodes.push(FileTreeNode {
            name,
            path,
            kind: FileTreeKind::File,
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
        lines.push(root.name.clone());
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
        lines.push(format!("{prefix}{connector}{}{suffix}", child.name));
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
        assert_eq!(TreeMode::from_arg(Some("bogus")), TreeMode::Auto);
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
