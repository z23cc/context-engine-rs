//! Structured, selection-aware flat file listing for UI file pickers / trees.
//!
//! `get_file_tree` renders an ASCII tree for an LLM; this returns the catalog's
//! files as STRUCTURED rows (root-relative `path` that `manage_selection`
//! accepts, a `display_path`, and whether the file is currently selected) so a
//! client can render a clickable, selectable tree. Pure + snapshot-backed.

use crate::{
    models::NerveError, port::CatalogProvider, selection::selection_key, snapshot::CatalogSnapshot,
};
use serde::{Deserialize, Serialize};

/// Default cap on returned rows (a large repo would otherwise flood a UI).
const DEFAULT_LIMIT: usize = 4000;

/// Request for the `list_files` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ListFilesRequest {
    /// Case-insensitive substring filter over the root-relative path.
    #[serde(default)]
    pub query: Option<String>,
    /// Maximum rows to return (default 4000).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One catalog file, with its selection state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListFile {
    pub root_id: String,
    /// Root-relative path — the value `manage_selection` add/remove accepts.
    pub path: String,
    pub display_path: String,
    pub selected: bool,
}

/// Structured response for `list_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ListFilesResponse {
    pub files: Vec<ListFile>,
    pub total: usize,
    pub truncated: bool,
}

/// List the catalog's files (selection-aware), sorted by display path.
pub fn list_files<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ListFilesRequest,
) -> Result<ListFilesResponse, NerveError> {
    let selection = provider.selection();
    let query = request.query.as_deref().map(str::to_ascii_lowercase);
    let limit = request.limit.unwrap_or(DEFAULT_LIMIT);

    let mut files: Vec<ListFile> = snapshot
        .entries
        .iter()
        .filter(|entry| match &query {
            Some(q) => entry.rel_path.to_ascii_lowercase().contains(q),
            None => true,
        })
        .map(|entry| ListFile {
            root_id: entry.root_id.clone(),
            path: entry.rel_path.clone(),
            display_path: provider.display_path(&entry.abs_path),
            selected: selection.files.contains_key(&selection_key(entry)),
        })
        .collect();

    files.sort_by(|a, b| a.display_path.cmp(&b.display_path));
    let total = files.len();
    let truncated = total > limit;
    files.truncate(limit);
    Ok(ListFilesResponse {
        files,
        total,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{
        ManageSelectionMode, ManageSelectionOp, ManageSelectionRequest, manage_selection,
    };
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions};
    use std::fs;
    use std::path::PathBuf;

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
}
