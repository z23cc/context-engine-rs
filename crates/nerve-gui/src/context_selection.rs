use crate::data::{SelectionFile, SelectionSummary};
use leptos::prelude::*;

const TYPEAHEAD_RESET_MS: f64 = 900.0;

#[derive(Clone, Default)]
struct SelectedTypeahead {
    text: String,
    at_ms: f64,
}

#[component]
pub(crate) fn SelectionManifest(
    selection: RwSignal<SelectionSummary>,
    copy_manifest: Callback<()>,
    remove_file: Callback<String>,
) -> impl IntoView {
    let active_row = RwSignal::new(0usize);
    let typeahead = RwSignal::new(SelectedTypeahead::default());

    Effect::new(move |_| {
        let len = selection.with(|summary| summary.files.len());
        active_row.update(|index| {
            if len == 0 {
                *index = 0;
            } else if *index >= len {
                *index = len - 1;
            }
        });
        typeahead.set(SelectedTypeahead::default());
    });

    let copy_manifest_label = move || {
        let count = selection.with(|summary| summary.files.len());
        format!("Copy selection manifest for {count} selected files")
    };

    let focus_row = move |index: usize| {
        active_row.set(index);
        crate::dom::focus_context_selected_row(index);
    };

    let typeahead_jump = move |key: String| -> bool {
        let rows = selection.with_untracked(|summary| summary.files.clone());
        if rows.is_empty() {
            return false;
        }
        let current = active_row.get_untracked().min(rows.len() - 1);
        let now_ms = js_sys::Date::now();
        let mut target = None;
        typeahead.update(|state| {
            target = selected_typeahead_target(&rows, current, state, &key, now_ms);
        });
        if let Some(next) = target {
            focus_row(next);
            return true;
        }
        false
    };

    view! {
        <div id="context-selection-manifest" class="ctx-selected">
            <div class="ctx-selected-head">
                <span>"Selection manifest"</span>
                <div class="ctx-selected-actions">
                    <button type="button" class="ctx-mini-btn" disabled=move || selection.with(|summary| summary.files.is_empty())
                        aria-label=copy_manifest_label
                        aria-controls="context-selection-list"
                        on:click=move |_| copy_manifest.run(())>"Copy manifest"</button>
                    <span class="ctx-selected-count">
                        {move || selection.with(|summary| {
                            format!("{} · {} tok", summary.files.len(), summary.total_tokens)
                        })}
                    </span>
                </div>
            </div>
            {move || {
                let summary = selection.get();
                if summary.files.is_empty() {
                    view! {
                        <div class="ctx-selected-empty" role="status">"No files selected."</div>
                    }.into_any()
                } else {
                    let rows = summary.files.clone();
                    let len = rows.len();
                    view! {
                        <>
                            <span id="context-selected-help" class="sr-only">
                                "Arrow keys move through selected files. Delete or Backspace removes the focused file. Escape returns to the file filter."
                            </span>
                            <div
                                id="context-selection-list"
                                class="ctx-selected-list"
                                role="list"
                                aria-label="Selected context files. Use arrow keys to move, type to jump, Delete to remove."
                                aria-describedby="context-selected-help"
                            >
                                {rows.into_iter().enumerate().map(|(index, file)| selected_row(
                                    file,
                                    index,
                                    len,
                                    active_row,
                                    focus_row,
                                    typeahead_jump,
                                    remove_file,
                                )).collect_view()}
                            </div>
                        </>
                    }.into_any()
                }
            }}
        </div>
    }
}

fn selected_row(
    file: SelectionFile,
    index: usize,
    len: usize,
    active_row: RwSignal<usize>,
    focus_row: impl Fn(usize) + Copy + 'static,
    typeahead_jump: impl Fn(String) -> bool + Copy + 'static,
    remove_file: Callback<String>,
) -> impl IntoView {
    let path = file.path.clone();
    let keydown_path = path.clone();
    let click_path = path.clone();
    let display_path = display_path(&file);
    let title_path = display_path.clone();
    let row_path = display_path.clone();
    let mode = selection_mode(&file);
    let tokens = file.token_estimate;
    let pos = (index + 1).to_string();
    let size = len.to_string();
    let label = format!(
        "Selected file {display_path}. {mode}. {tokens} tokens. Press Delete to remove from context."
    );

    view! {
        <div role="listitem">
            <button
                type="button"
                id=format!("context-selected-row-{index}")
                class="ctx-selected-row"
                class:active=move || active_row.get() == index
                title=title_path
                tabindex=move || if active_row.get() == index { "0" } else { "-1" }
                aria-label=label
                aria-posinset=pos
                aria-setsize=size
                aria-keyshortcuts="ArrowUp ArrowDown Home End Delete Backspace Escape"
                on:focus=move |_| active_row.set(index)
                on:keydown=move |ev| {
                    let key = ev.key();
                    if key == "Delete" || key == "Backspace" {
                        ev.prevent_default();
                        remove_file.run(keydown_path.clone());
                        return;
                    }
                    if key == "Escape" {
                        ev.prevent_default();
                        crate::dom::focus_context_filter();
                        return;
                    }
                    if let Some(next) = selected_row_target(index, len, &key) {
                        ev.prevent_default();
                        focus_row(next);
                        return;
                    }
                    if !(ev.meta_key() || ev.ctrl_key() || ev.alt_key()) && typeahead_jump(key) {
                        ev.prevent_default();
                    }
                }
                on:click=move |_| remove_file.run(click_path.clone())
            >
                <span class="ctx-selected-path">{row_path}</span>
                <span class="ctx-selected-meta">{mode}</span>
                <span class="ctx-selected-tokens">{tokens}</span>
                <span class="ctx-selected-rm">"Remove"</span>
            </button>
        </div>
    }
}

fn display_path(file: &SelectionFile) -> String {
    if file.display_path.is_empty() {
        file.path.clone()
    } else {
        file.display_path.clone()
    }
}

fn selection_mode(file: &SelectionFile) -> String {
    if let Some(range) = file.ranges.first() {
        let extra = file.ranges.len().saturating_sub(1);
        let label = range
            .label
            .as_ref()
            .map(|text| format!(" · {text}"))
            .unwrap_or_default();
        if extra == 0 {
            format!(
                "{} L{}-{}{}",
                file.mode, range.start_line, range.end_line, label
            )
        } else {
            format!(
                "{} L{}-{} +{}{}",
                file.mode, range.start_line, range.end_line, extra, label
            )
        }
    } else {
        file.mode.clone()
    }
}

fn selected_row_target(current: usize, len: usize, key: &str) -> Option<usize> {
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

fn selected_typeahead_target(
    rows: &[SelectionFile],
    current: usize,
    state: &mut SelectedTypeahead,
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

fn find_typeahead_match(rows: &[SelectionFile], start: usize, needle: &str) -> Option<usize> {
    if rows.is_empty() || needle.is_empty() {
        return None;
    }
    (0..rows.len())
        .map(|offset| (start + offset) % rows.len())
        .find(|&index| selected_file_matches(&rows[index], needle))
}

fn selected_file_matches(file: &SelectionFile, needle: &str) -> bool {
    let path = display_path(file).to_ascii_lowercase();
    let file_name = path.rsplit('/').next().unwrap_or(&path);
    file_name.starts_with(needle) || path.starts_with(needle)
}
