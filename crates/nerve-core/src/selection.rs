//! Persistent selection state and summaries.

use crate::{
    codemap::FileCodeStructure,
    models::{CatalogEntry, NerveError},
    path_match::{PathMatchInput, entry_child_match, entry_exact_match, entry_matches},
    port::CatalogProvider,
    selection_auto_codemap::auto_expand_codemap,
    snapshot::CatalogSnapshot,
    token::count_tokens,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Inclusive 1-based line range used by slice selections.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LineRange {
    pub start_line: usize,
    pub end_line: usize,
    #[serde(
        default,
        alias = "description",
        alias = "desc",
        skip_serializing_if = "Option::is_none"
    )]
    pub label: Option<String>,
}

impl LineRange {
    #[must_use]
    pub fn new(start_line: usize, end_line: usize) -> Self {
        Self {
            start_line,
            end_line,
            label: None,
        }
    }

    #[must_use]
    pub fn with_label(start_line: usize, end_line: usize, label: impl Into<String>) -> Self {
        Self {
            start_line,
            end_line,
            label: Some(label.into()),
        }
    }
}

/// Selection mode for one file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    Preview,
    Promote,
    Demote,
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
    /// When true, add up to eight codemap-only files defining symbols referenced
    /// by newly selected full/slice files. Explicit opt-in preserves precise
    /// manual selection budgets by default.
    #[serde(default)]
    pub auto_codemap: bool,
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
    /// True when `op=preview` returned a dry-run selection summary.
    #[serde(default, skip_serializing_if = "is_false")]
    pub preview: bool,
    /// True when the provider-owned persistent selection changed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mutated: bool,
    /// True when a previewed operation would change the persistent selection.
    #[serde(default, skip_serializing_if = "is_false")]
    pub would_mutate: bool,
    /// Number of codemap-only reference files auto-added by this request.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub auto_codemap_added: usize,
}

/// Apply a selection request and return a token-counted summary.
pub fn manage_selection<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) -> Result<ManageSelectionResponse, NerveError> {
    let current = provider.selection();
    let mut selection = current.clone();
    let commit = apply_selection_request(&mut selection, snapshot, request);
    let auto_codemap_added = auto_expand_codemap(provider, snapshot, &mut selection, request)?;
    let changed = selection != current;
    if commit && changed {
        provider.set_selection(selection.clone());
    }

    let mode_filter = if (request.op == ManageSelectionOp::Preview
        && has_selection_targets(request))
        || auto_codemap_added > 0
    {
        None
    } else {
        request.mode
    };
    let mut response = summarize_selection(provider, snapshot, &selection, mode_filter)?;
    response.preview = request.op == ManageSelectionOp::Preview;
    response.mutated = commit && changed;
    response.would_mutate = response.preview && changed;
    response.auto_codemap_added = auto_codemap_added;
    Ok(response)
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn apply_selection_request(
    selection: &mut Selection,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) -> bool {
    match request.op {
        ManageSelectionOp::Get => true,
        ManageSelectionOp::Preview => {
            if has_selection_targets(request) {
                add_targets(selection, snapshot, request);
            }
            false
        }
        ManageSelectionOp::Clear => {
            selection.files.clear();
            true
        }
        ManageSelectionOp::Add => {
            add_targets(selection, snapshot, request);
            true
        }
        ManageSelectionOp::Set => {
            selection.files.clear();
            add_targets(selection, snapshot, request);
            true
        }
        ManageSelectionOp::Remove => {
            remove_targets(selection, snapshot, request);
            true
        }
        ManageSelectionOp::Promote => {
            promote_targets(selection, snapshot, request);
            true
        }
        ManageSelectionOp::Demote => {
            demote_targets(selection, snapshot, request);
            true
        }
    }
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
    for key in target_keys(selection, snapshot, request) {
        selection.files.remove(&key);
    }
}

fn promote_targets(
    selection: &mut Selection,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) {
    for key in target_keys(selection, snapshot, request) {
        if let Some(mode) = selection.files.get_mut(&key) {
            *mode = SelectionMode::Full;
        }
    }
}

fn demote_targets(
    selection: &mut Selection,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) {
    for key in target_keys(selection, snapshot, request) {
        if let Some(mode) = selection.files.get_mut(&key) {
            *mode = SelectionMode::CodemapOnly;
        }
    }
}

pub(crate) fn target_keys(
    selection: &Selection,
    snapshot: &CatalogSnapshot,
    request: &ManageSelectionRequest,
) -> BTreeSet<SelectionKey> {
    if !has_selection_targets(request) {
        return selection.files.keys().cloned().collect();
    }
    let mut keys = BTreeSet::new();
    for entry in select_entries(snapshot, &request.paths) {
        keys.insert(selection_key(entry));
    }
    for slice in &request.slices {
        for entry in select_entries(snapshot, std::slice::from_ref(&slice.path)) {
            keys.insert(selection_key(entry));
        }
    }
    keys
}

pub(crate) fn has_selection_targets(request: &ManageSelectionRequest) -> bool {
    !request.paths.is_empty() || !request.slices.is_empty()
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
) -> Result<ManageSelectionResponse, NerveError> {
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
        preview: false,
        mutated: false,
        would_mutate: false,
        auto_codemap_added: 0,
    })
}

fn token_estimate_for_entry<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    mode: &SelectionMode,
) -> Result<usize, NerveError> {
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
        let input = PathMatchInput::from_path(path);
        for (idx, entry) in snapshot.entries.iter().enumerate() {
            if entry_matches(snapshot, entry, &input) {
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
    let input = PathMatchInput::from_path(path);
    let mut fallback = None;
    for entry in &snapshot.entries {
        if entry_exact_match(snapshot, entry, &input) {
            return Some(selection_key(entry));
        }
        if fallback.is_none() && entry_child_match(snapshot, entry, &input) {
            fallback = Some(selection_key(entry));
        }
    }
    fallback
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
    use serde_json::json;
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
}
