//! Diff and review cockpit panes for the right-side inspector.

use crate::clipboard::copy_text_with_note;
use leptos::prelude::*;

#[derive(Clone, Default)]
struct DiffStats {
    files: usize,
    additions: usize,
    deletions: usize,
    hunks: usize,
    has_tests: bool,
    is_clean: bool,
    changed_files: Vec<DiffFile>,
}

#[derive(Clone, Default)]
struct DiffFile {
    path: String,
    additions: usize,
    deletions: usize,
    hunks: usize,
}

#[derive(Clone, Default)]
struct ReviewTypeahead {
    text: String,
    at_ms: f64,
}

#[derive(Clone, Copy)]
struct ReviewRowCtx {
    len: usize,
    total: usize,
    active_row: RwSignal<usize>,
    copy_note: RwSignal<String>,
    owner_tab: &'static str,
}

const TYPEAHEAD_RESET_MS: f64 = 900.0;

pub(crate) fn changes_panel(data: RwSignal<String>) -> impl IntoView {
    let copy_note = RwSignal::new(String::new());
    view! {
        <div id="changes-panel" class="review-panel" role="tabpanel" aria-labelledby="inspector-tab-changes" aria-describedby="changes-shortcuts" tabindex="-1"
            aria-keyshortcuts="Meta+Shift+C Control+Shift+C"
            on:keydown=move |ev| {
                let key = ev.key();
                if (ev.meta_key() || ev.ctrl_key()) && ev.shift_key() && key.eq_ignore_ascii_case("c") {
                    ev.prevent_default();
                    copy_text_with_note(data.get_untracked(), copy_note, "Copied diff.");
                }
            }
        >
            {move || {
                let diff = data.get();
                let stats = diff_stats(&diff);
                view! {
                    <>
                        <section class="review-card">
                            <div class="review-card-head">
                                <span>"Changes"</span>
                                <div class="review-actions">
                                    <span class="review-card-subtitle">"working tree diff"</span>
                                    <button class="review-action quiet" type="button" aria-label="Copy working tree diff" aria-keyshortcuts="Meta+Shift+C Control+Shift+C" on:click={
                                        let diff = diff.clone();
                                        move |_| {
                                            copy_text_with_note(diff.clone(), copy_note, "Copied diff.");
                                        }
                                    }>"Copy diff"</button>
                                </div>
                            </div>
                            {move || (!copy_note.get().is_empty()).then(|| view! {
                                <span class="review-copy-note" role="status">{copy_note.get()}</span>
                            })}
                            <div id="changes-shortcuts" class="review-shortcuts" aria-label="Changes keyboard shortcuts">
                                <span><kbd>"⌘/Ctrl⇧C"</kbd>" Copy diff"</span>
                            </div>
                            {review_summary(stats.clone())}
                            {review_files(stats.changed_files.clone(), diff.clone(), copy_note, "changes")}
                        </section>
                        <div class="review-preview-head">"Unified diff"</div>
                        <pre class="inspector-pre review-preview" aria-label="Unified diff preview">{diff}</pre>
                    </>
                }
            }}
        </div>
    }
}

pub(crate) fn review_panel(
    data: RwSignal<String>,
    draft_review: Callback<String>,
) -> impl IntoView {
    let copy_note = RwSignal::new(String::new());
    let draft_review_keys = draft_review;
    view! {
        <div id="review-panel" class="review-panel" role="tabpanel" aria-labelledby="inspector-tab-review" aria-describedby="review-shortcuts" tabindex="-1"
            aria-keyshortcuts="Meta+Enter Control+Enter Meta+Shift+C Control+Shift+C"
            on:keydown=move |ev| {
                let key = ev.key();
                let chord = ev.meta_key() || ev.ctrl_key();
                if !chord {
                    return;
                }
                let diff = data.get_untracked();
                let stats = diff_stats(&diff);
                if key == "Enter" {
                    ev.prevent_default();
                    draft_review_keys.run(review_prompt(&stats));
                } else if ev.shift_key() && key.eq_ignore_ascii_case("c") {
                    ev.prevent_default();
                    copy_text_with_note(
                        review_packet(&stats, &diff),
                        copy_note,
                        "Copied review packet.",
                    );
                }
            }
        >
            {move || {
                let diff = data.get();
                let stats = diff_stats(&diff);
                view! {
                    <>
                        <section class="review-card">
                            <div class="review-card-head">
                                <span>"Diff Review"</span>
                                <div class="review-actions">
                                    <button class="review-action quiet" type="button" aria-label="Copy current diff" on:click={
                                        let diff = diff.clone();
                                        move |_| {
                                            copy_text_with_note(diff.clone(), copy_note, "Copied diff.");
                                        }
                                    }>"Copy diff"</button>
                                    <button class="review-action quiet" type="button" aria-label="Copy review prompt" on:click={
                                        let stats = stats.clone();
                                        move |_| {
                                            copy_text_with_note(
                                                review_prompt(&stats),
                                                copy_note,
                                                "Copied review prompt.",
                                            );
                                        }
                                    }>"Copy prompt"</button>
                                    <button class="review-action quiet" type="button" aria-label="Copy review packet" aria-keyshortcuts="Meta+Shift+C Control+Shift+C" on:click={
                                        let stats = stats.clone();
                                        let diff = diff.clone();
                                        move |_| {
                                            copy_text_with_note(
                                                review_packet(&stats, &diff),
                                                copy_note,
                                                "Copied review packet.",
                                            );
                                        }
                                    }>"Copy packet"</button>
                                    <button class="review-action" type="button" aria-label="Draft review prompt in composer" aria-keyshortcuts="Meta+Enter Control+Enter" on:click={
                                        let stats = stats.clone();
                                        move |_| draft_review.run(review_prompt(&stats))
                                    }>"Draft prompt"</button>
                                </div>
                            </div>
                            {move || (!copy_note.get().is_empty()).then(|| view! {
                                <span class="review-copy-note" role="status">{copy_note.get()}</span>
                            })}
                            <div id="review-shortcuts" class="review-shortcuts" aria-label="Review keyboard shortcuts">
                                <span><kbd>"⌘/Ctrl↵"</kbd>" Draft prompt"</span>
                                <span><kbd>"⌘/Ctrl⇧C"</kbd>" Copy packet"</span>
                            </div>
                            {review_summary(stats.clone())}
                            {review_files(stats.changed_files.clone(), diff.clone(), copy_note, "review")}
                            <ul class="review-list">
                                {review_risks(&stats).into_iter().map(|risk| view! { <li>{risk}</li> }).collect_view()}
                            </ul>
                        </section>
                        <div class="review-preview-head">"Current diff preview"</div>
                        <pre class="inspector-pre review-preview" aria-label="Current diff preview">{diff}</pre>
                    </>
                }
            }}
        </div>
    }
}

pub(crate) fn diff_review_packet(diff: &str) -> String {
    let stats = diff_stats(diff);
    review_packet(&stats, diff)
}

fn review_packet(stats: &DiffStats, diff: &str) -> String {
    let diff = if diff.trim().is_empty() {
        "No diff available."
    } else {
        diff
    };
    format!(
        "# Diff review packet\n\n## Review prompt\n\n{}\n\n## Review checklist\n\n{}\n\n## Unified diff\n\n```diff\n{}\n```",
        review_prompt(stats),
        review_risks(stats)
            .into_iter()
            .map(|risk| format!("- {risk}"))
            .collect::<Vec<_>>()
            .join("\n"),
        diff
    )
}

fn review_prompt(stats: &DiffStats) -> String {
    let files = if stats.changed_files.is_empty() {
        "- No changed files detected".to_string()
    } else {
        stats
            .changed_files
            .iter()
            .take(12)
            .map(|file| {
                format!(
                    "- {} (+{} -{}, {} hunks)",
                    file.path, file.additions, file.deletions, file.hunks
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Review the current git diff for correctness, risks, missing tests, and simplification opportunities. Start with the highest-impact findings.\n\nDiff summary: {} files, +{}, -{}, {} hunks.\nChanged files:\n{}\n\nFocus on concrete bugs, stale state, async races, missing coverage, and smaller-diff alternatives.",
        stats.files, stats.additions, stats.deletions, stats.hunks, files
    )
}

fn review_summary(stats: DiffStats) -> impl IntoView {
    view! {
        <div class="review-stats">
            <div class="review-stat">
                <span class="review-stat-n">{stats.files}</span>
                <span class="review-stat-label">"files"</span>
            </div>
            <div class="review-stat add">
                <span class="review-stat-n">{stats.additions}</span>
                <span class="review-stat-label">"added"</span>
            </div>
            <div class="review-stat del">
                <span class="review-stat-n">{stats.deletions}</span>
                <span class="review-stat-label">"removed"</span>
            </div>
            <div class="review-stat">
                <span class="review-stat-n">{stats.hunks}</span>
                <span class="review-stat-label">"hunks"</span>
            </div>
        </div>
    }
}

fn review_files(
    files: Vec<DiffFile>,
    diff: String,
    copy_note: RwSignal<String>,
    owner_tab: &'static str,
) -> AnyView {
    if files.is_empty() {
        return ().into_any();
    }
    let total = files.len();
    let rows = files;
    let len = rows.len();
    let active_row = RwSignal::new(0usize);
    let typeahead = RwSignal::new(ReviewTypeahead::default());
    let rows_for_typeahead = rows.clone();
    let focus_row = move |index: usize| {
        active_row.set(index);
        crate::dom::focus_review_file_row(owner_tab, index);
    };
    let typeahead_jump = move |key: String| -> bool {
        if rows_for_typeahead.is_empty() {
            return false;
        }
        let current = active_row.get_untracked().min(rows_for_typeahead.len() - 1);
        let now_ms = js_sys::Date::now();
        let mut target = None;
        typeahead.update(|state| {
            target = review_typeahead_target(&rows_for_typeahead, current, state, &key, now_ms);
        });
        if let Some(next) = target {
            focus_row(next);
            return true;
        }
        false
    };
    let row_ctx = ReviewRowCtx {
        len,
        total,
        active_row,
        copy_note,
        owner_tab,
    };
    let help_id = format!("review-files-help-{owner_tab}");
    let list_id = format!("review-file-list-{owner_tab}");
    view! {
        <div class="review-files">
            <div class="review-files-head">
                <span>"Changed files"</span>
                <span>{total}</span>
            </div>
            <span id=help_id.clone() class="sr-only">
                "Arrow keys move through changed files. Type to jump. Enter or Space copies the focused file diff. Escape returns to the Inspector tabs."
            </span>
            <div id=list_id class="review-file-list" role="list" aria-label="Changed files" aria-describedby=help_id>
                {rows.into_iter().enumerate().map(|(index, file)| review_file_row(
                    file,
                    index,
                    row_ctx,
                    focus_row,
                    typeahead_jump.clone(),
                    diff.clone(),
                )).collect_view()}
            </div>
        </div>
    }
    .into_any()
}

fn review_file_row(
    file: DiffFile,
    index: usize,
    ctx: ReviewRowCtx,
    focus_row: impl Fn(usize) + Copy + 'static,
    typeahead_jump: impl Fn(String) -> bool + Clone + 'static,
    diff: String,
) -> impl IntoView {
    let path = file.path.clone();
    let button_title = path.clone();
    let path_title = path.clone();
    let display_path = path.clone();
    let click_path = path.clone();
    let key_path = path.clone();
    let click_diff = diff.clone();
    let key_diff = diff;
    let pos = (index + 1).to_string();
    let size = ctx.total.to_string();
    let label = format!(
        "Changed file {path}. Plus {}, minus {}, {} hunks. Press Enter to copy this file diff.",
        file.additions, file.deletions, file.hunks
    );
    view! {
        <div role="listitem" aria-posinset=pos aria-setsize=size>
            <button
                type="button"
                id=format!("review-file-row-{}-{index}", ctx.owner_tab)
                class="review-file-row"
                class:active=move || ctx.active_row.get() == index
                title=button_title
                tabindex=move || if ctx.active_row.get() == index { "0" } else { "-1" }
                aria-label=label
                aria-keyshortcuts="ArrowUp ArrowDown Home End Enter Space Escape"
                on:focus=move |_| ctx.active_row.set(index)
                on:keydown=move |ev| {
                    let key = ev.key();
                    if key == "Enter" || key == " " {
                        ev.prevent_default();
                        copy_file_diff(&key_diff, &key_path, ctx.copy_note);
                        return;
                    }
                    if key == "Escape" {
                        ev.prevent_default();
                        crate::dom::focus_inspector_tab(ctx.owner_tab);
                        return;
                    }
                    if let Some(next) = review_row_target(index, ctx.len, &key) {
                        ev.prevent_default();
                        focus_row(next);
                        return;
                    }
                    if !(ev.meta_key() || ev.ctrl_key() || ev.alt_key()) && typeahead_jump(key) {
                        ev.prevent_default();
                    }
                }
                on:click=move |_| copy_file_diff(&click_diff, &click_path, ctx.copy_note)
            >
                <span class="review-file-path" title=path_title>{display_path}</span>
                <span class="review-file-add">{format!("+{}", file.additions)}</span>
                <span class="review-file-del">{format!("-{}", file.deletions)}</span>
                <span class="review-file-hunks">{format!("{}h", file.hunks)}</span>
            </button>
        </div>
    }
}

fn copy_file_diff(diff: &str, path: &str, copy_note: RwSignal<String>) {
    copy_text_with_note(
        file_diff(diff, path),
        copy_note,
        format!("Copied diff for {path}."),
    );
}

fn file_diff(diff: &str, path: &str) -> String {
    let mut capture = false;
    let mut lines = Vec::new();
    for line in diff.lines() {
        if let Some(next_path) = diff_path(line) {
            if capture {
                break;
            }
            capture = next_path == path;
        }
        if capture {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        format!("No diff found for {path}.")
    } else {
        lines.join("\n")
    }
}

fn review_row_target(current: usize, len: usize, key: &str) -> Option<usize> {
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

fn review_typeahead_target(
    rows: &[DiffFile],
    current: usize,
    state: &mut ReviewTypeahead,
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
    if let Some(index) = find_review_file_match(rows, start, &state.text) {
        return Some(index);
    }
    state.text.clear();
    state.text.push(ch.to_ascii_lowercase());
    find_review_file_match(rows, current.saturating_add(1), &state.text)
}

fn printable_typeahead_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() || ch.is_whitespace() {
        return None;
    }
    Some(ch)
}

fn find_review_file_match(rows: &[DiffFile], start: usize, needle: &str) -> Option<usize> {
    if rows.is_empty() || needle.is_empty() {
        return None;
    }
    (0..rows.len())
        .map(|offset| (start + offset) % rows.len())
        .find(|&index| review_file_matches(&rows[index], needle))
}

fn review_file_matches(file: &DiffFile, needle: &str) -> bool {
    let path = file.path.to_ascii_lowercase();
    let file_name = path.rsplit('/').next().unwrap_or(&path);
    file_name.starts_with(needle) || path.starts_with(needle)
}

fn review_risks(stats: &DiffStats) -> Vec<&'static str> {
    if stats.is_clean {
        return vec!["Working tree is clean."];
    }
    if stats.files == 0 && stats.additions == 0 && stats.deletions == 0 {
        return vec!["Diff is loading or unavailable."];
    }
    let mut risks = vec!["Correctness: edge cases, regressions, invariants"];
    if !stats.has_tests {
        risks.push("Tests: no test file changes detected");
    }
    if stats.files >= 10 || stats.additions + stats.deletions >= 500 {
        risks.push("Scope: large diff, review by subsystem first");
    }
    if stats.hunks >= 20 {
        risks.push("Risk: many edit hunks, watch for stale state and partial refactors");
    }
    risks.push("Simplicity: smaller diff, clearer seams");
    risks
}

fn diff_stats(diff: &str) -> DiffStats {
    let trimmed = diff.trim();
    if trimmed.is_empty() || trimmed == "No changes in the working tree." || trimmed == "loading…"
    {
        return DiffStats {
            is_clean: trimmed != "loading…",
            ..DiffStats::default()
        };
    }

    let mut stats = DiffStats::default();
    let mut current_file = None::<usize>;
    for line in trimmed.lines() {
        if let Some(path) = diff_path(line) {
            stats.has_tests |= is_test_path(&path);
            stats.changed_files.push(DiffFile {
                path,
                ..DiffFile::default()
            });
            current_file = stats.changed_files.len().checked_sub(1);
        } else if line.starts_with("@@") {
            stats.hunks += 1;
            if let Some(idx) = current_file {
                stats.changed_files[idx].hunks += 1;
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            stats.additions += 1;
            if let Some(idx) = current_file {
                stats.changed_files[idx].additions += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            stats.deletions += 1;
            if let Some(idx) = current_file {
                stats.changed_files[idx].deletions += 1;
            }
        }
    }
    stats.files = stats.changed_files.len();
    stats
}

fn diff_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if let Some((_, right)) = rest.split_once(" b/") {
        return Some(clean_diff_path(right));
    }
    if let Some((_, right)) = rest.split_once("\" \"b/") {
        return Some(clean_diff_path(right));
    }
    let mut parts = rest.split_whitespace();
    let first = parts.next()?;
    let second = parts.next().unwrap_or(first);
    Some(clean_diff_path(second))
}

fn clean_diff_path(path: &str) -> String {
    path.trim_matches('"')
        .strip_prefix("b/")
        .or_else(|| path.trim_matches('"').strip_prefix("a/"))
        .unwrap_or_else(|| path.trim_matches('"'))
        .to_string()
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec") || lower.contains("snapshot")
}

#[cfg(test)]
mod tests {
    use super::{diff_stats, file_diff};

    #[test]
    fn parses_unquoted_diff_paths_with_spaces() {
        let diff = "diff --git a/src/foo bar.rs b/src/foo bar.rs\nindex 1..2 100644\n--- a/src/foo bar.rs\n+++ b/src/foo bar.rs\n@@ -1 +1 @@\n-old\n+new";
        let stats = diff_stats(diff);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.changed_files[0].path, "src/foo bar.rs");
        assert!(file_diff(diff, "src/foo bar.rs").contains("+new"));
    }

    #[test]
    fn parses_quoted_diff_paths_with_spaces() {
        let diff = "diff --git \"a/src/foo bar.rs\" \"b/src/foo bar.rs\"\nindex 1..2 100644\n--- a/src/foo bar.rs\n+++ b/src/foo bar.rs\n@@ -1 +1 @@\n-old\n+new";
        let stats = diff_stats(diff);
        assert_eq!(stats.files, 1);
        assert_eq!(stats.changed_files[0].path, "src/foo bar.rs");
        assert!(file_diff(diff, "src/foo bar.rs").contains("-old"));
    }
}
