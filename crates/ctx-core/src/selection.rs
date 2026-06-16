//! Persistent selection state and summaries.

use crate::{
    codemap::FileCodeStructure,
    models::{CatalogEntry, CtxError},
    port::CatalogProvider,
    snapshot::CatalogSnapshot,
    token::count_tokens,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Inclusive 1-based line range used by slice selections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRange {
    pub start_line: usize,
    pub end_line: usize,
}

/// Selection mode for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionMode {
    Full,
    Slices(Vec<LineRange>),
    CodemapOnly,
}

/// Stable key for a selected catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SelectionKey {
    pub root_id: String,
    pub path: String,
}

/// Persistent file selection owned by the engine/provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub files: BTreeMap<SelectionKey, SelectionMode>,
}

/// Operation accepted by `manage_selection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManageSelectionOp {
    Get,
    Add,
    Remove,
    Set,
    Clear,
}

/// String mode accepted by `manage_selection` arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManageSelectionMode {
    Full,
    Slices,
    CodemapOnly,
}

/// One explicit slice target in a `manage_selection` call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SelectionSliceArg {
    pub path: PathBuf,
    #[serde(default)]
    pub ranges: Vec<LineRange>,
}

/// Transport-neutral request for selection mutations.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManageSelectionRequest {
    pub op: ManageSelectionOp,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    pub mode: Option<ManageSelectionMode>,
    #[serde(default)]
    pub slices: Vec<SelectionSliceArg>,
}

/// Summary for one selected file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionFileSummary {
    pub root_id: String,
    pub path: String,
    pub display_path: String,
    pub mode: String,
    pub ranges: Vec<LineRange>,
    pub token_estimate: usize,
}

/// Summary returned by `manage_selection`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManageSelectionResponse {
    pub files: Vec<SelectionFileSummary>,
    pub total_tokens: usize,
}

/// Apply a selection request and return a token-counted summary.
pub fn manage_selection<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) -> Result<ManageSelectionResponse, CtxError> {
    let mut selection = provider.selection();

    match request.op {
        ManageSelectionOp::Get => {}
        ManageSelectionOp::Clear => {
            selection.files.clear();
            provider.set_selection(selection.clone());
        }
        ManageSelectionOp::Add => {
            add_targets(&mut selection, snapshot, request);
            provider.set_selection(selection.clone());
        }
        ManageSelectionOp::Set => {
            selection.files.clear();
            add_targets(&mut selection, snapshot, request);
            provider.set_selection(selection.clone());
        }
        ManageSelectionOp::Remove => {
            remove_targets(&mut selection, snapshot, request);
            provider.set_selection(selection.clone());
        }
    }

    summarize_selection(provider, snapshot, &selection, request.mode)
}

fn add_targets(
    selection: &mut Selection,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) {
    let default_mode = mode_from_arg(
        request.mode.unwrap_or(ManageSelectionMode::Full),
        Vec::new(),
    );
    for entry in select_entries(snapshot, &request.paths) {
        selection
            .files
            .insert(selection_key(entry), default_mode.clone());
    }
    for slice in &request.slices {
        for entry in select_entries(snapshot, std::slice::from_ref(&slice.path)) {
            selection.files.insert(
                selection_key(entry),
                SelectionMode::Slices(slice.ranges.clone()),
            );
        }
    }
}

fn remove_targets(
    selection: &mut Selection,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) {
    let mut keys = BTreeSet::new();
    for entry in select_entries(snapshot, &request.paths) {
        keys.insert(selection_key(entry));
    }
    for slice in &request.slices {
        for entry in select_entries(snapshot, std::slice::from_ref(&slice.path)) {
            keys.insert(selection_key(entry));
        }
    }
    for key in keys {
        selection.files.remove(&key);
    }
}

fn mode_from_arg(mode: ManageSelectionMode, ranges: Vec<LineRange>) -> SelectionMode {
    match mode {
        ManageSelectionMode::Full => SelectionMode::Full,
        ManageSelectionMode::Slices => SelectionMode::Slices(ranges),
        ManageSelectionMode::CodemapOnly => SelectionMode::CodemapOnly,
    }
}

fn summarize_selection<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &Selection,
    mode_filter: Option<ManageSelectionMode>,
) -> Result<ManageSelectionResponse, CtxError> {
    let entries_by_key = snapshot
        .entries
        .iter()
        .map(|entry| (selection_key(entry), entry))
        .collect::<BTreeMap<_, _>>();
    let mut files = Vec::new();

    for (key, mode) in &selection.files {
        if mode_filter.is_some_and(|filter| !mode_matches(mode, filter)) {
            continue;
        }
        let Some(entry) = entries_by_key.get(key) else {
            continue;
        };
        let token_estimate = token_estimate_for_entry(provider, entry, mode)?;
        files.push(SelectionFileSummary {
            root_id: key.root_id.clone(),
            path: key.path.clone(),
            display_path: provider.display_path(&entry.abs_path),
            mode: mode_name(mode).to_string(),
            ranges: mode_ranges(mode),
            token_estimate,
        });
    }

    let total_tokens = files.iter().map(|file| file.token_estimate).sum();
    Ok(ManageSelectionResponse {
        files,
        total_tokens,
    })
}

fn token_estimate_for_entry<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    mode: &SelectionMode,
) -> Result<usize, CtxError> {
    match mode {
        SelectionMode::Full => {
            let bytes = provider.read_bytes(&entry.abs_path)?;
            Ok(count_tokens(&String::from_utf8_lossy(&bytes)))
        }
        SelectionMode::Slices(ranges) => {
            let bytes = provider.read_bytes(&entry.abs_path)?;
            let text = String::from_utf8_lossy(&bytes);
            Ok(count_tokens(&slice_text(&text, ranges)))
        }
        SelectionMode::CodemapOnly => {
            let Some(parsed) = provider
                .code_symbols_for_path(&entry.abs_path, &entry.rel_path)?
                .ok()
                .flatten()
            else {
                return Ok(0);
            };
            let structure = FileCodeStructure {
                path: entry.rel_path.clone(),
                language: parsed.language.clone(),
                symbols: parsed.symbols.clone(),
                token_count: 0,
            };
            let text = serde_json::to_string(&structure).expect("codemap summary serializes");
            Ok(count_tokens(&text))
        }
    }
}

fn slice_text(text: &str, ranges: &[LineRange]) -> String {
    let line_segments: Vec<&str> = text.split_inclusive('\n').collect();
    if line_segments.is_empty() {
        return String::new();
    }
    let mut selected = String::new();
    for range in ranges {
        let start = range.start_line.max(1).min(line_segments.len());
        let end = range.end_line.max(start).min(line_segments.len());
        selected.push_str(&line_segments[start - 1..end].concat());
    }
    selected
}

fn select_entries<'a>(snapshot: &'a CatalogSnapshot, paths: &[PathBuf]) -> Vec<&'a CatalogEntry> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut selected = BTreeSet::new();
    for path in paths {
        let (rel, canonical) = path_match_inputs(path);
        for (idx, entry) in snapshot.entries.iter().enumerate() {
            let rel_match = rel.is_empty()
                || entry.rel_path == rel
                || entry.rel_path.starts_with(&format!("{rel}/"));
            let abs_match = canonical
                .as_ref()
                .is_some_and(|abs| entry.abs_path == *abs || entry.abs_path.starts_with(abs));
            if rel_match || abs_match {
                selected.insert(idx);
            }
        }
    }
    selected
        .into_iter()
        .map(|idx| &snapshot.entries[idx])
        .collect()
}

pub(crate) fn selection_key_for_path(
    snapshot: &CatalogSnapshot,
    path: &Path,
) -> Option<SelectionKey> {
    let (rel, canonical) = path_match_inputs(path);
    let mut fallback = None;
    for entry in &snapshot.entries {
        let rel_exact = entry.rel_path == rel;
        let abs_exact = canonical.as_ref().is_some_and(|abs| entry.abs_path == *abs);
        if rel_exact || abs_exact {
            return Some(selection_key(entry));
        }
        let rel_child = !rel.is_empty() && entry.rel_path.starts_with(&format!("{rel}/"));
        let abs_child = canonical
            .as_ref()
            .is_some_and(|abs| entry.abs_path.starts_with(abs));
        if fallback.is_none() && (rel_child || abs_child) {
            fallback = Some(selection_key(entry));
        }
    }
    fallback
}

fn path_match_inputs(path: &Path) -> (String, Option<PathBuf>) {
    let raw = path.to_string_lossy().replace('\\', "/");
    let rel = raw
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string();
    (rel, canonicalize_existing(path))
}

fn canonicalize_existing(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}

pub(crate) fn selection_key(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

fn mode_name(mode: &SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Full => "full",
        SelectionMode::Slices(_) => "slices",
        SelectionMode::CodemapOnly => "codemap_only",
    }
}

fn mode_ranges(mode: &SelectionMode) -> Vec<LineRange> {
    match mode {
        SelectionMode::Slices(ranges) => ranges.clone(),
        SelectionMode::Full | SelectionMode::CodemapOnly => Vec::new(),
    }
}

fn mode_matches(mode: &SelectionMode, filter: ManageSelectionMode) -> bool {
    matches!(
        (mode, filter),
        (SelectionMode::Full, ManageSelectionMode::Full)
            | (SelectionMode::Slices(_), ManageSelectionMode::Slices)
            | (SelectionMode::CodemapOnly, ManageSelectionMode::CodemapOnly)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions};
    use std::fs;

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
                    }],
                }],
            },
        )
        .expect("set slice");
        assert_eq!(set_slice.files[0].mode, "slices");
        assert_eq!(set_slice.files[0].ranges[0].start_line, 2);
        assert!(set_slice.total_tokens < add.total_tokens);

        let removed = manage_selection(
            &provider,
            &snapshot,
            &ManageSelectionRequest {
                op: ManageSelectionOp::Remove,
                paths: vec![PathBuf::from("a.txt")],
                mode: None,
                slices: Vec::new(),
            },
        )
        .expect("remove");
        assert!(removed.files.is_empty());
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
            },
        )
        .expect("codemap selection");
        assert_eq!(summary.files[0].mode, "codemap_only");
        assert!(summary.files[0].token_estimate > 0);
    }
}
