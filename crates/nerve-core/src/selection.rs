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
