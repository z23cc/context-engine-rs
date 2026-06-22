//! Deterministic rebasing for persisted line-slice selections.

use crate::{
    CatalogProvider,
    edit::snapshot_tag,
    selection::{self, LineRange, SelectionKey, SelectionMode},
};
use std::{collections::BTreeSet, path::Path};

/// Result of rebasing 1-based inclusive line ranges from old text to new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceRebaseResult {
    pub rebased: Vec<LineRange>,
    pub dropped: Vec<LineRange>,
    pub did_change: bool,
}

/// Rebase selected line ranges across a text mutation.
#[must_use]
pub fn rebase_ranges(old_text: &str, new_text: &str, ranges: &[LineRange]) -> SliceRebaseResult {
    let old_lines = normalized_lines(old_text);
    let new_lines = normalized_lines(new_text);
    let normalized = normalize_ranges(ranges, old_lines.len());
    if normalized.is_empty() {
        return SliceRebaseResult {
            rebased: Vec::new(),
            dropped: Vec::new(),
            did_change: false,
        };
    }
    if new_lines.is_empty() {
        return SliceRebaseResult {
            rebased: Vec::new(),
            dropped: normalized,
            did_change: true,
        };
    }

    let prefix = common_prefix_len(&old_lines, &new_lines);
    let suffix = common_suffix_len(&old_lines[prefix..], &new_lines[prefix..]);
    let old_changed_end = old_lines.len().saturating_sub(suffix);
    let delta = new_lines.len() as isize - old_lines.len() as isize;
    let mut rebased = Vec::new();
    let mut dropped = Vec::new();

    for range in &normalized {
        if range.end_line <= prefix {
            rebased.push(range.clone());
        } else if range.start_line > old_changed_end {
            rebased.push(clamp_range(shift_range(range, delta), new_lines.len()));
        } else if let Some(anchored) = anchor_rebase(range, &old_lines, &new_lines) {
            rebased.push(anchored);
        } else {
            dropped.push(range.clone());
        }
    }

    rebased = normalize_ranges(&rebased, new_lines.len());
    let did_change = rebased != normalized || !dropped.is_empty();
    SliceRebaseResult {
        rebased,
        dropped,
        did_change,
    }
}

fn normalized_lines(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn normalize_ranges(ranges: &[LineRange], line_count: usize) -> Vec<LineRange> {
    if line_count == 0 {
        return Vec::new();
    }
    let mut normalized: Vec<LineRange> = ranges
        .iter()
        .map(|range| {
            let start = range.start_line.max(1).min(line_count);
            let end = range.end_line.max(start).min(line_count);
            LineRange {
                start_line: start,
                end_line: end,
                label: range.label.clone(),
            }
        })
        .collect();
    normalized.sort_by_key(|range| (range.start_line, range.end_line));
    merge_ranges(normalized)
}

fn merge_ranges(ranges: Vec<LineRange>) -> Vec<LineRange> {
    let mut merged: Vec<LineRange> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut()
            && range.start_line <= last.end_line.saturating_add(1)
            && range.label == last.label
        {
            last.end_line = last.end_line.max(range.end_line);
            continue;
        }
        merged.push(range);
    }
    merged
}

fn common_prefix_len(old_lines: &[String], new_lines: &[String]) -> usize {
    old_lines
        .iter()
        .zip(new_lines)
        .take_while(|(old, new)| old == new)
        .count()
}

fn common_suffix_len(old_tail: &[String], new_tail: &[String]) -> usize {
    old_tail
        .iter()
        .rev()
        .zip(new_tail.iter().rev())
        .take_while(|(old, new)| old == new)
        .count()
}

fn shift_range(range: &LineRange, delta: isize) -> LineRange {
    LineRange {
        start_line: shift_line(range.start_line, delta),
        end_line: shift_line(range.end_line, delta),
        label: range.label.clone(),
    }
}

fn shift_line(line: usize, delta: isize) -> usize {
    if delta.is_negative() {
        line.saturating_sub(delta.unsigned_abs()).max(1)
    } else {
        line.saturating_add(delta as usize)
    }
}

fn clamp_range(range: LineRange, line_count: usize) -> LineRange {
    let start = range.start_line.max(1).min(line_count);
    let end = range.end_line.max(start).min(line_count);
    LineRange {
        start_line: start,
        end_line: end,
        label: range.label,
    }
}

fn anchor_rebase(
    range: &LineRange,
    old_lines: &[String],
    new_lines: &[String],
) -> Option<LineRange> {
    let starts = boundary_candidates(old_lines, new_lines, range.start_line);
    let ends = boundary_candidates(old_lines, new_lines, range.end_line);
    let mut best: Option<(usize, usize, usize)> = None;
    for start in starts {
        for end in &ends {
            if start > *end {
                continue;
            }
            let distance = start.abs_diff(range.start_line) + end.abs_diff(range.end_line);
            let candidate = (distance, start, *end);
            if best.is_none_or(|current| candidate < current) {
                best = Some(candidate);
            }
        }
    }
    best.map(|(_, start, end)| {
        clamp_range(
            LineRange {
                start_line: start,
                end_line: end,
                label: range.label.clone(),
            },
            new_lines.len(),
        )
    })
}

fn boundary_candidates(old_lines: &[String], new_lines: &[String], boundary: usize) -> Vec<usize> {
    let mut candidates = BTreeSet::new();
    for len in (1..=3).rev() {
        for start in window_starts(boundary, len, old_lines.len()) {
            let end = start + len - 1;
            let offset = boundary - start;
            let window = &old_lines[start - 1..end];
            for matched in find_window(new_lines, window) {
                candidates.insert(matched + offset);
            }
        }
    }
    candidates.into_iter().collect()
}

fn window_starts(boundary: usize, len: usize, line_count: usize) -> Vec<usize> {
    if boundary == 0 || boundary > line_count || len > line_count {
        return Vec::new();
    }
    let min_start = boundary.saturating_sub(len - 1).max(1);
    let max_start = boundary.min(line_count - len + 1);
    (min_start..=max_start).collect()
}

fn find_window(new_lines: &[String], window: &[String]) -> Vec<usize> {
    if window.is_empty() || window.len() > new_lines.len() {
        return Vec::new();
    }
    let tag = snapshot_tag(&window.join("\n"));
    new_lines
        .windows(window.len())
        .enumerate()
        .filter_map(|(idx, candidate)| {
            (candidate == window && snapshot_tag(&candidate.join("\n")) == tag).then_some(idx + 1)
        })
        .collect()
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct SelectionMutation {
    pub(crate) old_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) new_path: Option<String>,
    pub(crate) mode_before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) mode_after: Option<String>,
    pub(crate) ranges_before: Vec<LineRange>,
    pub(crate) ranges_after: Vec<LineRange>,
    pub(crate) dropped: Vec<LineRange>,
    pub(crate) did_change: bool,
}

pub(crate) fn selected_key<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: &str,
) -> Option<SelectionKey> {
    provider
        .snapshot()
        .ok()
        .and_then(|snapshot| selection::selection_key_for_path(&snapshot, Path::new(path)))
}

pub(crate) fn update_selection_for_content<P: CatalogProvider + ?Sized>(
    provider: &P,
    before_key: Option<SelectionKey>,
    after_key: Option<SelectionKey>,
    old_path: &str,
    new_path: &str,
    old_text: &str,
    new_text: &str,
) -> Option<SelectionMutation> {
    let before_key = before_key?;
    let after_key = after_key.unwrap_or_else(|| fallback_key(&before_key, new_path));
    let mut selection = provider.selection();
    let mode = selection.files.remove(&before_key)?;
    let key_changed = before_key != after_key;
    let report = match mode {
        SelectionMode::Slices(ranges) => {
            let result = rebase_ranges(old_text, new_text, &ranges);
            let mode_after = (!result.rebased.is_empty()).then_some("slices".to_string());
            if !result.rebased.is_empty() {
                selection
                    .files
                    .insert(after_key, SelectionMode::Slices(result.rebased.clone()));
            }
            Some(selection_report(SelectionReportInput {
                old_path,
                new_path,
                mode_before: "slices",
                mode_after,
                ranges_before: ranges,
                ranges_after: result.rebased,
                dropped: result.dropped,
                did_change: result.did_change || key_changed,
            }))
        }
        other => {
            selection.files.insert(after_key, other.clone());
            key_changed.then(|| preserve_report(old_path, new_path, &other))
        }
    };
    provider.set_selection(selection);
    report
}

pub(crate) fn remove_selection<P: CatalogProvider + ?Sized>(
    provider: &P,
    before_key: Option<SelectionKey>,
) -> Option<SelectionMutation> {
    let before_key = before_key?;
    let mut selection = provider.selection();
    let mode = selection.files.remove(&before_key)?;
    provider.set_selection(selection);
    Some(selection_report(SelectionReportInput {
        old_path: &before_key.path,
        new_path: &before_key.path,
        mode_before: mode_name(&mode),
        mode_after: None,
        ranges_before: mode_ranges(&mode),
        ranges_after: Vec::new(),
        dropped: mode_ranges(&mode),
        did_change: true,
    }))
}

pub(crate) fn transfer_selection<P: CatalogProvider + ?Sized>(
    provider: &P,
    before_key: Option<SelectionKey>,
    after_key: Option<SelectionKey>,
    old_path: &str,
    new_path: &str,
) -> Option<SelectionMutation> {
    let before_key = before_key?;
    let after_key = after_key.unwrap_or_else(|| fallback_key(&before_key, new_path));
    let mut selection = provider.selection();
    let mode = selection.files.remove(&before_key)?;
    let key_changed = before_key != after_key;
    selection.files.insert(after_key, mode.clone());
    provider.set_selection(selection);
    key_changed.then(|| preserve_report(old_path, new_path, &mode))
}

fn preserve_report(old_path: &str, new_path: &str, mode: &SelectionMode) -> SelectionMutation {
    selection_report(SelectionReportInput {
        old_path,
        new_path,
        mode_before: mode_name(mode),
        mode_after: Some(mode_name(mode).to_string()),
        ranges_before: mode_ranges(mode),
        ranges_after: mode_ranges(mode),
        dropped: Vec::new(),
        did_change: true,
    })
}

struct SelectionReportInput<'a> {
    old_path: &'a str,
    new_path: &'a str,
    mode_before: &'a str,
    mode_after: Option<String>,
    ranges_before: Vec<LineRange>,
    ranges_after: Vec<LineRange>,
    dropped: Vec<LineRange>,
    did_change: bool,
}

fn selection_report(input: SelectionReportInput<'_>) -> SelectionMutation {
    SelectionMutation {
        old_path: normalize_path_text(input.old_path),
        new_path: (input.old_path != input.new_path).then(|| normalize_path_text(input.new_path)),
        mode_before: input.mode_before.to_string(),
        mode_after: input.mode_after,
        ranges_before: input.ranges_before,
        ranges_after: input.ranges_after,
        dropped: input.dropped,
        did_change: input.did_change,
    }
}

fn fallback_key(before_key: &SelectionKey, path: &str) -> SelectionKey {
    SelectionKey {
        root_id: before_key.root_id.clone(),
        path: normalize_path_text(path),
    }
}

fn normalize_path_text(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start_line: usize, end_line: usize) -> LineRange {
        LineRange {
            start_line,
            end_line,
            label: None,
        }
    }

    fn labeled_range(start_line: usize, end_line: usize, label: &str) -> LineRange {
        LineRange {
            start_line,
            end_line,
            label: Some(label.to_string()),
        }
    }

    #[test]
    fn insertion_before_slice_shifts_range_down() {
        let result = rebase_ranges("a\nb\nc\n", "x\na\nb\nc\n", &[labeled_range(2, 3, "focus")]);
        assert_eq!(result.rebased, vec![labeled_range(3, 4, "focus")]);
        assert!(result.dropped.is_empty());
        assert!(result.did_change);
    }

    #[test]
    fn deletion_before_slice_shifts_range_up() {
        let result = rebase_ranges("x\na\nb\nc\n", "a\nb\nc\n", &[range(3, 4)]);
        assert_eq!(result.rebased, vec![range(2, 3)]);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn overlapping_edit_uses_boundary_anchor_fallback() {
        let old = "a\nstart\nmiddle\nend\nz\n";
        let new = "a\nstart\ninserted\nmiddle\nend\nz\n";
        let result = rebase_ranges(old, new, &[range(2, 4)]);
        assert_eq!(result.rebased, vec![range(2, 5)]);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn unresolvable_range_is_dropped() {
        let result = rebase_ranges("a\nb\nc\n", "x\ny\nz\n", &[range(2, 2)]);
        assert!(result.rebased.is_empty());
        assert_eq!(result.dropped, vec![range(2, 2)]);
    }

    #[test]
    fn empty_new_file_drops_all_ranges() {
        let result = rebase_ranges("a\nb\n", "", &[range(1, 2)]);
        assert!(result.rebased.is_empty());
        assert_eq!(result.dropped, vec![range(1, 2)]);
    }

    #[test]
    fn duplicate_anchor_windows_choose_nearest_then_lowest() {
        let old = "dup\na\ndup\n";
        let new = "dup\ndup\na\ndup\n";
        let result = rebase_ranges(old, new, &[range(2, 2)]);
        assert_eq!(result.rebased, vec![range(3, 3)]);
    }
}
