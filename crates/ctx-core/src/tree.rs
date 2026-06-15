//! Compact file tree rendering from a snapshot.

use crate::{models::*, snapshot::CatalogSnapshot};
use std::collections::BTreeMap;

#[derive(Default)]
struct NodeBuilder {
    files: BTreeMap<String, String>,
    dirs: BTreeMap<String, NodeBuilder>,
}

/// Build a compact file tree from snapshot paths.
#[must_use]
pub fn get_file_tree(snapshot: &CatalogSnapshot, max_depth: usize) -> FileTreeResponse {
    let mut roots = Vec::new();
    let mut omitted = 0usize;

    for root in &snapshot.roots {
        let mut builder = NodeBuilder::default();
        for entry in snapshot
            .entries
            .iter()
            .filter(|entry| entry.root_id == root.id)
        {
            insert(&mut builder, &entry.rel_path, &entry.rel_path);
        }
        let children = materialize(builder, 0, max_depth, &mut omitted);
        roots.push(FileTreeNode {
            name: root
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            path: String::new(),
            kind: FileTreeKind::Directory,
            children,
        });
    }

    let tree = render_ascii_tree(&roots);
    FileTreeResponse {
        roots_count: roots.len(),
        was_truncated: omitted > 0,
        uses_legend: false,
        roots,
        tree,
        omitted,
    }
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

fn materialize(
    builder: NodeBuilder,
    depth: usize,
    max_depth: usize,
    omitted: &mut usize,
) -> Vec<FileTreeNode> {
    let mut nodes = Vec::new();
    for (name, child) in builder.dirs {
        if depth >= max_depth {
            *omitted += count_builder(&child);
            continue;
        }
        nodes.push(FileTreeNode {
            path: name.clone(),
            name,
            kind: FileTreeKind::Directory,
            children: materialize(child, depth + 1, max_depth, omitted),
        });
    }
    for (name, path) in builder.files {
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
    builder.files.len() + builder.dirs.values().map(count_builder).sum::<usize>()
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
