//! Right-side inspector: thread-wide tool activity plus files/changes panes.

use crate::clipboard::copy_text_with_note;
use crate::session_inspector::sessions_panel;
use crate::{
    app::Chat,
    diff_review::{changes_panel, review_panel},
    trace_format::tool_trace,
};
use leptos::prelude::*;

#[derive(Clone)]
struct ToolRow {
    index: usize,
    tool: String,
    status: &'static str,
    label: &'static str,
    input: String,
    input_preview: String,
    input_meta: String,
    output: String,
    output_preview: String,
    output_meta: String,
}

#[derive(Clone, Copy, Default)]
struct ToolStats {
    total: usize,
    ok: usize,
    err: usize,
    run: usize,
}

const INSPECTOR_TABS: &[&str] = &["plan", "sessions", "files", "changes", "review"];
const TOOL_FILTERS: &[&str] = &["all", "err", "run", "ok"];

#[component]
pub(crate) fn Inspector(
    branch: RwSignal<String>,
    tab: RwSignal<&'static str>,
    data: RwSignal<String>,
    token: StoredValue<Option<String>>,
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    load_tab: Callback<&'static str>,
    close_inspector: Callback<()>,
    draft_review: Callback<String>,
) -> impl IntoView {
    let tool_filter = RwSignal::new("all");
    let active_tool = RwSignal::new(0usize);
    let tool_note = RwSignal::new(String::new());
    view! {
        <aside id="inspector-panel" class="inspector" role="complementary" aria-label="Inspector"
            on:keydown=move |ev| {
                if ev.key() == "Escape" {
                    ev.prevent_default();
                    close_inspector.run(());
                }
            }
        >
            <div class="inspector-head">
                <span class="inspector-title">"Inspector"</span>
                <span class="inspector-chip">"Esc close"</span>
                <span class="inspector-chip">"⎇ "{move || branch.get()}</span>
            </div>
            <div
                class="inspector-tabs"
                role="tablist"
                aria-label="Inspector panes"
                on:keydown=move |ev| {
                    if let Some(next) = tab_for_key(tab.get_untracked(), &ev.key(), INSPECTOR_TABS) {
                        ev.prevent_default();
                        load_tab.run(next);
                        crate::dom::focus_inspector_tab(next);
                    }
                }
            >
                <button type="button" id="inspector-tab-plan" class="inspector-tab" class:on=move || tab.get() == "plan"
                    role="tab" aria-controls="tool-panel" aria-selected=move || selected_attr(tab, "plan")
                    tabindex=move || tab_index_attr(tab, "plan")
                    on:click=move |_| load_tab.run("plan")>"Tools"</button>
                <button type="button" id="inspector-tab-sessions" class="inspector-tab" class:on=move || tab.get() == "sessions"
                    role="tab" aria-controls="sessions-panel" aria-selected=move || selected_attr(tab, "sessions")
                    tabindex=move || tab_index_attr(tab, "sessions")
                    on:click=move |_| load_tab.run("sessions")>"Agents"</button>
                <button type="button" id="inspector-tab-files" class="inspector-tab" class:on=move || tab.get() == "files"
                    role="tab" aria-controls="files-panel" aria-selected=move || selected_attr(tab, "files")
                    tabindex=move || tab_index_attr(tab, "files")
                    on:click=move |_| load_tab.run("files")>"Files"</button>
                <button type="button" id="inspector-tab-changes" class="inspector-tab" class:on=move || tab.get() == "changes"
                    role="tab" aria-controls="changes-panel" aria-selected=move || selected_attr(tab, "changes")
                    tabindex=move || tab_index_attr(tab, "changes")
                    on:click=move |_| load_tab.run("changes")>"Changes"</button>
                <button type="button" id="inspector-tab-review" class="inspector-tab" class:on=move || tab.get() == "review"
                    role="tab" aria-controls="review-panel" aria-selected=move || selected_attr(tab, "review")
                    tabindex=move || tab_index_attr(tab, "review")
                    on:click=move |_| load_tab.run("review")>"Review"</button>
            </div>
            <div class="inspector-body">
                {move || if tab.get() == "plan" {
                    let rows = tool_rows(chats, active);
                    let empty = rows.is_empty();
                    let stats = tool_stats(&rows);
                    let packet = tool_activity_packet(&rows);
                    let packet_for_keys = packet.clone();
                    let filter = tool_filter.get();
                    let visible = filter_rows(rows, filter);
                    let visible_len = visible.len();
                    view! {
                        <div id="tool-panel" class="tool-panel" role="tabpanel" aria-labelledby="inspector-tab-plan" aria-describedby="tool-shortcuts" tabindex="-1"
                            aria-keyshortcuts="Meta+Shift+C Control+Shift+C 1 2 3 4"
                            on:keydown=move |ev| handle_tool_panel_key(ev, &packet_for_keys, tool_filter, active_tool, tool_note, visible_len)
                        >
                            {tool_summary(stats, filter, tool_filter, active_tool, packet, tool_note)}
                            {tool_shortcuts()}
                            {if empty {
                                view! { <div id="tool-timeline" class="plan-empty" role="status">"No tool activity in this thread yet."</div> }.into_any()
                            } else if visible.is_empty() {
                                view! { <div id="tool-timeline" class="plan-empty" role="status">"No tool calls match this filter."</div> }.into_any()
                            } else {
                                view! {
                                    {tool_timeline(visible, active_tool, tool_note)}
                                }.into_any()
                            }}
                        </div>
                    }.into_any()
                } else if tab.get() == "sessions" {
                    sessions_panel(token, chats, active).into_any()
                } else if tab.get() == "changes" {
                    changes_panel(data).into_any()
                } else if tab.get() == "review" {
                    review_panel(data, draft_review).into_any()
                } else {
                    files_panel(data).into_any()
                }}
            </div>
        </aside>
    }
}

fn files_panel(data: RwSignal<String>) -> impl IntoView {
    let copy_note = RwSignal::new(String::new());
    view! {
        <div id="files-panel" class="files-panel" role="tabpanel" aria-labelledby="inspector-tab-files" aria-describedby="files-shortcuts" tabindex="-1"
            aria-keyshortcuts="Meta+Shift+C Control+Shift+C"
            on:keydown=move |ev| {
                let key = ev.key();
                if (ev.meta_key() || ev.ctrl_key()) && ev.shift_key() && key.eq_ignore_ascii_case("c") {
                    ev.prevent_default();
                    copy_text_with_note(data.get_untracked(), copy_note, "Copied file tree.");
                }
            }
        >
            <section class="files-card">
                <div class="files-card-head">
                    <span>"Files"</span>
                    <div class="review-actions">
                        <span class="review-card-subtitle">"workspace tree"</span>
                        <button class="review-action quiet" type="button" aria-label="Copy workspace file tree" aria-keyshortcuts="Meta+Shift+C Control+Shift+C" on:click={
                            move |_| {
                                copy_text_with_note(data.get_untracked(), copy_note, "Copied file tree.");
                            }
                        }>"Copy tree"</button>
                    </div>
                </div>
                {move || (!copy_note.get().is_empty()).then(|| view! {
                    <span class="review-copy-note" role="status">{copy_note.get()}</span>
                })}
                <div id="files-shortcuts" class="review-shortcuts" aria-label="Files keyboard shortcuts">
                    <span><kbd>"⌘/Ctrl⇧C"</kbd>" Copy tree"</span>
                </div>
            </section>
            <pre class="inspector-pre files-preview" aria-label="Workspace file tree preview">{move || data.get()}</pre>
        </div>
    }
}

fn selected_attr(tab: RwSignal<&'static str>, id: &'static str) -> &'static str {
    if tab.get() == id { "true" } else { "false" }
}

fn tab_index_attr(tab: RwSignal<&'static str>, id: &'static str) -> &'static str {
    if tab.get() == id { "0" } else { "-1" }
}

fn tab_for_key(
    current: &'static str,
    key: &str,
    tabs: &'static [&'static str],
) -> Option<&'static str> {
    let idx = tabs.iter().position(|id| *id == current)?;
    match key {
        "ArrowRight" => tabs.get((idx + 1) % tabs.len()).copied(),
        "ArrowLeft" => tabs.get((idx + tabs.len() - 1) % tabs.len()).copied(),
        "Home" => tabs.first().copied(),
        "End" => tabs.last().copied(),
        _ => None,
    }
}

fn tool_rows(chats: RwSignal<Vec<Chat>>, active: RwSignal<usize>) -> Vec<ToolRow> {
    chats.with(|cs| {
        cs.get(active.get())
            .map(|chat| {
                chat.turns
                    .iter()
                    .flat_map(|turn| turn.tools.iter())
                    .enumerate()
                    .map(|(idx, card)| {
                        let (status, label) = match card.ok {
                            None => ("run", "running"),
                            Some(true) => ("ok", "ok"),
                            Some(false) => ("err", "error"),
                        };
                        ToolRow {
                            index: idx + 1,
                            tool: card.tool.clone(),
                            status,
                            label,
                            input: card.input.clone(),
                            input_preview: preview(&card.input),
                            input_meta: text_meta(&card.input),
                            output: card.output.clone(),
                            output_preview: preview(&card.output),
                            output_meta: text_meta(&card.output),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn tool_activity_packet(rows: &[ToolRow]) -> String {
    let stats = tool_stats(rows);
    let mut sections = vec![format!(
        "# Tool activity\n\n{} tool calls · {} ok · {} errors · {} running",
        stats.total, stats.ok, stats.err, stats.run
    )];
    for row in rows {
        let trace = if row.input.trim().is_empty() && row.output.trim().is_empty() {
            "No trace yet.".to_string()
        } else {
            tool_trace(&row.tool, row.label, &row.input, &row.output)
        };
        sections.push(format!(
            "## #{:02} {} — {}\n\n{}",
            row.index, row.tool, row.label, trace
        ));
    }
    sections.join("\n\n")
}

fn tool_stats(rows: &[ToolRow]) -> ToolStats {
    rows.iter().fold(
        ToolStats {
            total: rows.len(),
            ..ToolStats::default()
        },
        |mut stats, row| {
            match row.status {
                "ok" => stats.ok += 1,
                "err" => stats.err += 1,
                "run" => stats.run += 1,
                _ => {}
            }
            stats
        },
    )
}

fn filter_rows(mut rows: Vec<ToolRow>, filter: &'static str) -> Vec<ToolRow> {
    if filter != "all" {
        rows.retain(|row| row.status == filter);
    }
    rows
}

fn handle_tool_panel_key(
    ev: leptos::ev::KeyboardEvent,
    packet: &str,
    filter: RwSignal<&'static str>,
    active_tool: RwSignal<usize>,
    note: RwSignal<String>,
    visible_len: usize,
) {
    let key = ev.key();
    if (ev.meta_key() || ev.ctrl_key()) && ev.shift_key() && key.eq_ignore_ascii_case("c") {
        ev.prevent_default();
        copy_text_with_note(packet.to_string(), note, "Copied tool activity.");
        return;
    }
    let unmodified = !(ev.meta_key() || ev.ctrl_key() || ev.alt_key());
    if unmodified && let Some(next) = tool_filter_for_number(key.as_str()) {
        ev.prevent_default();
        active_tool.set(0);
        filter.set(next);
        crate::dom::focus_tool_filter(next);
        return;
    }
    if unmodified
        && let Some(next) = tool_row_for_nav(active_tool.get_untracked(), visible_len, &key)
    {
        ev.prevent_default();
        active_tool.set(next);
        crate::dom::focus_tool_row(next);
    }
}

fn tool_summary(
    stats: ToolStats,
    current: &'static str,
    filter: RwSignal<&'static str>,
    active_tool: RwSignal<usize>,
    packet: String,
    note: RwSignal<String>,
) -> impl IntoView {
    view! {
        <div class="tool-summary-card">
            <div class="tool-summary-line">
                <span class="tool-summary-title">{format!("{} tool calls", stats.total)}</span>
                <span class="tool-summary-metric err">{format!("{} errors", stats.err)}</span>
                <span class="tool-summary-metric run">{format!("{} running", stats.run)}</span>
                <button class="tool-summary-copy" type="button" aria-label="Copy tool activity packet" aria-keyshortcuts="Meta+Shift+C Control+Shift+C" on:click={
                    let packet = packet.clone();
                    move |_| {
                        copy_text_with_note(packet.clone(), note, "Copied tool activity.");
                    }
                }>"Copy activity"</button>
            </div>
            {move || (!note.get().is_empty()).then(|| view! {
                <span class="tool-summary-note" role="status">{note.get()}</span>
            })}
            <div class="tool-filters" role="radiogroup" aria-label="Filter tool calls" aria-describedby="tool-shortcuts"
                aria-keyshortcuts="ArrowLeft ArrowRight ArrowUp ArrowDown Home End 1 2 3 4"
                on:keydown=move |ev| {
                    if let Some(next) = tool_filter_for_nav(filter.get_untracked(), &ev.key()) {
                        ev.prevent_default();
                        ev.stop_propagation();
                        active_tool.set(0);
                        filter.set(next);
                        crate::dom::focus_tool_filter(next);
                    }
                }>
                {filter_button("all", "All", stats.total, current, filter, active_tool)}
                {filter_button("err", "Errors", stats.err, current, filter, active_tool)}
                {filter_button("run", "Running", stats.run, current, filter, active_tool)}
                {filter_button("ok", "OK", stats.ok, current, filter, active_tool)}
            </div>
        </div>
    }
}

fn tool_shortcuts() -> impl IntoView {
    view! {
        <div id="tool-shortcuts" class="tool-shortcuts" aria-label="Tool activity keyboard shortcuts">
            <span><kbd>"⌘/Ctrl⇧C"</kbd>" Copy activity"</span>
            <span><kbd>"←/→"</kbd>" Filter"</span>
            <span><kbd>"↑/↓"</kbd>" Tool row"</span>
            <span><kbd>"↵"</kbd>" Expand"</span>
            <span><kbd>"1"</kbd>" All"</span>
            <span><kbd>"2"</kbd>" Errors"</span>
            <span><kbd>"3"</kbd>" Running"</span>
            <span><kbd>"4"</kbd>" OK"</span>
        </div>
    }
}

fn tool_filter_for_number(key: &str) -> Option<&'static str> {
    match key {
        "1" => Some("all"),
        "2" => Some("err"),
        "3" => Some("run"),
        "4" => Some("ok"),
        _ => None,
    }
}

fn tool_filter_for_nav(current: &'static str, key: &str) -> Option<&'static str> {
    let idx = TOOL_FILTERS.iter().position(|id| *id == current)?;
    match key {
        "ArrowRight" | "ArrowDown" => TOOL_FILTERS.get((idx + 1) % TOOL_FILTERS.len()).copied(),
        "ArrowLeft" | "ArrowUp" => TOOL_FILTERS
            .get((idx + TOOL_FILTERS.len() - 1) % TOOL_FILTERS.len())
            .copied(),
        "Home" => TOOL_FILTERS.first().copied(),
        "End" => TOOL_FILTERS.last().copied(),
        _ => None,
    }
}

fn tool_row_for_nav(current: usize, len: usize, key: &str) -> Option<usize> {
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

fn tool_filter_shortcut(id: &'static str) -> &'static str {
    match id {
        "all" => "1",
        "err" => "2",
        "run" => "3",
        "ok" => "4",
        _ => "",
    }
}

fn filter_button(
    id: &'static str,
    label: &'static str,
    count: usize,
    current: &'static str,
    filter: RwSignal<&'static str>,
    active_tool: RwSignal<usize>,
) -> impl IntoView {
    view! {
        <button id=format!("tool-filter-{id}") class="tool-filter" type="button" class:on=current == id
            role="radio"
            aria-checked=if current == id { "true" } else { "false" }
            aria-controls="tool-timeline"
            aria-keyshortcuts=tool_filter_shortcut(id)
            tabindex=if current == id { "0" } else { "-1" }
            aria-label=format!("Show {label} tool calls, {count}")
            on:click=move |_| {
                active_tool.set(0);
                filter.set(id);
            }>
            <span>{label}</span>
            <span class="tool-filter-count">{count}</span>
        </button>
    }
}

fn tool_timeline(
    rows: Vec<ToolRow>,
    active_tool: RwSignal<usize>,
    note: RwSignal<String>,
) -> impl IntoView {
    let len = rows.len();
    view! {
        <>
            <span id="tool-row-help" class="sr-only">
                "Arrow keys move through tool calls. Enter or Space expands a focused trace. Copy trace copies the focused tool input and output."
            </span>
            <div id="tool-timeline" class="tool-timeline" role="list" aria-label="Tool call timeline" aria-describedby="tool-shortcuts tool-row-help">
                {rows.into_iter().enumerate().map(|(index, row)| tool_row(
                    row,
                    index,
                    len,
                    active_tool,
                    note,
                )).collect_view()}
            </div>
        </>
    }
}

fn tool_row(
    row: ToolRow,
    visible_index: usize,
    visible_len: usize,
    active_tool: RwSignal<usize>,
    note: RwSignal<String>,
) -> impl IntoView {
    let has_input = !row.input.trim().is_empty();
    let has_output = !row.output.trim().is_empty();
    let row_label = format!("Tool call #{:02}: {}, {}", row.index, row.tool, row.label);
    let pos = (visible_index + 1).to_string();
    let size = visible_len.to_string();
    let is_active = move || active_tool.get().min(visible_len.saturating_sub(1)) == visible_index;
    if !has_input && !has_output {
        return view! {
            <article id=format!("tool-row-focus-{visible_index}") class=format!("tool-row {}", row.status)
                class:active=is_active role="listitem" tabindex=move || if is_active() { "0" } else { "-1" }
                aria-label=row_label aria-posinset=pos aria-setsize=size
                aria-keyshortcuts="ArrowUp ArrowDown Home End"
                on:focus=move |_| active_tool.set(visible_index)>
                {tool_head(row.index, row.tool.clone(), row.status, row.label)}
                <div class="tool-row-empty" role="status">"No trace yet."</div>
            </article>
        }
        .into_any();
    }

    let trace = tool_trace(&row.tool, row.label, &row.input, &row.output);
    let trace_for_copy = trace.clone();
    let copy_note = format!("Copied trace for #{:02} {}.", row.index, row.tool);
    let copy_label = format!("Copy trace for tool call #{:02} {}", row.index, row.tool);
    let input_preview = row.input_preview.clone();
    let input_meta = row.input_meta.clone();
    let input = row.input.clone();
    let output_preview = row.output_preview.clone();
    let output_meta = row.output_meta.clone();
    let output = row.output.clone();

    view! {
        <details class=format!("tool-row {}", row.status) class:active=is_active
            role="listitem" aria-label=row_label aria-posinset=pos aria-setsize=size>
            <summary id=format!("tool-row-focus-{visible_index}") class="tool-row-summary"
                tabindex=move || if is_active() { "0" } else { "-1" }
                aria-keyshortcuts="ArrowUp ArrowDown Home End Enter Space"
                on:focus=move |_| active_tool.set(visible_index)>
                {tool_head(row.index, row.tool.clone(), row.status, row.label)}
                <div class="tool-row-previews">
                    {has_input.then(|| preview_line("input", input_preview.clone(), input_meta.clone()))}
                    {has_output.then(|| preview_line("output", output_preview.clone(), output_meta.clone()))}
                </div>
            </summary>
            <button class="tool-row-copy" type="button" aria-label=copy_label on:click=move |_| {
                copy_text_with_note(trace_for_copy.clone(), note, copy_note.clone());
            }>"Copy trace"</button>
            {has_input.then(|| io_block("Input", input.clone()))}
            {has_output.then(|| io_block("Output", output.clone()))}
        </details>
    }
    .into_any()
}

fn preview_line(label: &'static str, text: String, meta: String) -> impl IntoView {
    view! {
        <div class="tool-row-preview">
            <span class="tool-row-preview-label">{label}</span>
            <span class="tool-row-preview-text">{text}</span>
            <span class="tool-row-meta">{meta}</span>
        </div>
    }
}

fn io_block(label: &'static str, text: String) -> impl IntoView {
    view! {
        <section class="tool-io">
            <div class="tool-io-label">{label}</div>
            <pre class="tool-row-output" aria-label=format!("{label} trace")>{text}</pre>
        </section>
    }
}

fn tool_head(
    index: usize,
    tool: String,
    status: &'static str,
    label: &'static str,
) -> impl IntoView {
    view! {
        <div class="tool-row-head">
            <span class=format!("tool-dot {status}") title=label aria-label=label></span>
            <span class="tool-row-name">{tool}</span>
            <span class=format!("tool-row-status {status}")>{label}</span>
            <span class="tool-row-index">{format!("#{index:02}")}</span>
        </div>
    }
}

fn preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= 220 {
        return trimmed.to_string();
    }
    let mut out = trimmed.chars().take(220).collect::<String>();
    out.push('…');
    out
}

fn text_meta(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "0 lines · 0 chars".to_string();
    }
    let lines = trimmed.lines().count();
    let chars = trimmed.chars().count();
    format!("{lines} lines · {chars} chars")
}
