use crate::{models::CatalogEntry, snapshot::CatalogSnapshot};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct PathMatchInput {
    rel: String,
    canonical: Option<PathBuf>,
}

impl PathMatchInput {
    pub(crate) fn from_path(path: &Path) -> Self {
        let raw = path.to_string_lossy().replace('\\', "/");
        let rel = raw
            .trim_start_matches("./")
            .trim_end_matches('/')
            .to_string();
        Self {
            rel,
            canonical: path.canonicalize().ok(),
        }
    }
}

pub(crate) fn entry_matches(
    snapshot: &CatalogSnapshot,
    entry: &CatalogEntry,
    input: &PathMatchInput,
) -> bool {
    entry_exact_match(snapshot, entry, input) || entry_child_match(snapshot, entry, input)
}

pub(crate) fn entry_exact_match(
    snapshot: &CatalogSnapshot,
    entry: &CatalogEntry,
    input: &PathMatchInput,
) -> bool {
    if snapshot
        .entries
        .iter()
        .any(|entry| literal_exact(entry, input))
    {
        return literal_exact(entry, input);
    }
    scoped_rel(entry, snapshot, &input.rel).is_some_and(|rel| rel_exact(entry, rel))
}

pub(crate) fn entry_child_match(
    snapshot: &CatalogSnapshot,
    entry: &CatalogEntry,
    input: &PathMatchInput,
) -> bool {
    if snapshot
        .entries
        .iter()
        .any(|entry| literal_child(entry, input))
    {
        return literal_child(entry, input);
    }
    scoped_rel(entry, snapshot, &input.rel).is_some_and(|rel| rel_child(entry, rel))
}

fn literal_exact(entry: &CatalogEntry, input: &PathMatchInput) -> bool {
    rel_exact(entry, &input.rel)
        || input
            .canonical
            .as_ref()
            .is_some_and(|abs| entry.abs_path == *abs)
}

fn literal_child(entry: &CatalogEntry, input: &PathMatchInput) -> bool {
    rel_child(entry, &input.rel)
        || input
            .canonical
            .as_ref()
            .is_some_and(|abs| entry.abs_path.starts_with(abs))
}

fn rel_exact(entry: &CatalogEntry, rel: &str) -> bool {
    !rel.is_empty() && entry.rel_path == rel
}

fn rel_child(entry: &CatalogEntry, rel: &str) -> bool {
    !rel.is_empty() && entry.rel_path.starts_with(&format!("{rel}/"))
}

fn scoped_rel<'a>(
    entry: &CatalogEntry,
    snapshot: &CatalogSnapshot,
    rel: &'a str,
) -> Option<&'a str> {
    let (prefix, remainder) = rel.split_once('/').unwrap_or((rel, ""));
    if prefix.is_empty() {
        return None;
    }
    if prefix == entry.root_id {
        return Some(remainder);
    }
    let mut matching_roots = snapshot
        .roots
        .iter()
        .filter(|root| root_name(root.path.as_path()) == prefix);
    let root = matching_roots.next()?;
    if matching_roots.next().is_some() || root.id != entry.root_id {
        return None;
    }
    Some(remainder)
}

fn root_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RootRef, models::CatalogEntry, snapshot::CatalogSnapshot};
    use std::path::PathBuf;

    fn snapshot() -> CatalogSnapshot {
        CatalogSnapshot {
            generation: 1,
            roots: vec![
                RootRef {
                    id: "root-0".to_string(),
                    path: PathBuf::from("/repo/left"),
                },
                RootRef {
                    id: "root-1".to_string(),
                    path: PathBuf::from("/repo/right"),
                },
            ],
            entries: vec![
                entry("root-0", "common.txt", "/repo/left/common.txt"),
                entry(
                    "root-0",
                    "root-0/common.txt",
                    "/repo/left/root-0/common.txt",
                ),
                entry("root-1", "common.txt", "/repo/right/common.txt"),
            ],
            diagnostics: Vec::new(),
        }
    }

    fn entry(root_id: &str, rel_path: &str, abs_path: &str) -> CatalogEntry {
        CatalogEntry {
            root_id: root_id.to_string(),
            rel_path: rel_path.to_string(),
            abs_path: PathBuf::from(abs_path),
            size: 1,
        }
    }

    #[test]
    fn root_name_prefix_matches_only_that_root() {
        let snap = snapshot();
        let input = PathMatchInput::from_path(Path::new("right/common.txt"));
        assert!(!entry_matches(&snap, &snap.entries[0], &input));
        assert!(!entry_matches(&snap, &snap.entries[1], &input));
        assert!(entry_matches(&snap, &snap.entries[2], &input));
    }

    #[test]
    fn root_id_prefix_matches_only_that_root() {
        let snap = snapshot();
        let input = PathMatchInput::from_path(Path::new("root-0/common.txt"));
        assert!(!entry_matches(&snap, &snap.entries[0], &input));
        assert!(entry_matches(&snap, &snap.entries[1], &input));
        assert!(!entry_matches(&snap, &snap.entries[2], &input));
    }

    #[test]
    fn empty_path_matches_no_entries() {
        let snap = snapshot();
        let input = PathMatchInput::from_path(Path::new(""));
        assert!(
            snap.entries
                .iter()
                .all(|entry| !entry_matches(&snap, entry, &input))
        );
    }
}
