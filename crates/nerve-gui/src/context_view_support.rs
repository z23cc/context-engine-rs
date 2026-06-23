use crate::data::FileRow;

const TYPEAHEAD_RESET_MS: f64 = 900.0;

#[derive(Clone, Default)]
pub(crate) struct TypeaheadState {
    text: String,
    at_ms: f64,
}

pub(crate) fn context_typeahead_target(
    rows: &[FileRow],
    current: usize,
    state: &mut TypeaheadState,
    key: &str,
    now_ms: f64,
) -> Option<usize> {
    let ch = printable_typeahead_char(key)?;
    let continuing = !state.text.is_empty() && now_ms - state.at_ms <= TYPEAHEAD_RESET_MS;
    if !continuing {
        state.text.clear();
    }
    state.at_ms = now_ms;
    state.text.push(ch.to_ascii_lowercase());

    let start = if continuing {
        current
    } else {
        current.saturating_add(1)
    };
    if let Some(index) = find_typeahead_match(rows, start, &state.text) {
        return Some(index);
    }
    state.text.clear();
    state.text.push(ch.to_ascii_lowercase());
    find_typeahead_match(rows, current.saturating_add(1), &state.text)
}

fn printable_typeahead_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() || ch.is_whitespace() {
        return None;
    }
    Some(ch)
}

fn find_typeahead_match(rows: &[FileRow], start: usize, needle: &str) -> Option<usize> {
    if rows.is_empty() || needle.is_empty() {
        return None;
    }
    (0..rows.len())
        .map(|offset| (start + offset) % rows.len())
        .find(|&index| row_matches_typeahead(&rows[index], needle))
}

fn row_matches_typeahead(row: &FileRow, needle: &str) -> bool {
    let path = row.display_path.to_ascii_lowercase();
    let file_name = path.rsplit('/').next().unwrap_or(&path);
    file_name.starts_with(needle) || path.starts_with(needle)
}

pub(crate) fn context_row_target(current: usize, len: usize, key: &str) -> Option<usize> {
    (len != 0)
        .then(|| match key {
            "ArrowDown" => Some((current + 1).min(len - 1)),
            "ArrowUp" => Some(current.saturating_sub(1)),
            "Home" => Some(0),
            "End" => Some(len - 1),
            _ => None,
        })
        .flatten()
}

pub(crate) fn visible_matching_count(
    rows: &[FileRow],
    selected_only: bool,
    selected: bool,
) -> usize {
    rows.iter()
        .filter(|row| (!selected_only || row.selected) && row.selected == selected)
        .count()
}

pub(crate) fn visible_files(rows: &[FileRow], selected_only: bool) -> Vec<FileRow> {
    rows.iter()
        .filter(|row| !selected_only || row.selected)
        .cloned()
        .collect()
}
