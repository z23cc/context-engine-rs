//! The sidebar: brand, New thread, a conversation search box, the project row,
//! the searchable + recency-sorted + timestamped conversation rail, and a footer
//! with Settings + runtime status. Owns new-thread / close-thread (sidebar
//! actions). Split out of `app.rs` to stay under the file-size gate.

// View-heavy component; see app.rs for why too_many_lines is allowed at module
// scope (the `#[component]` macro drops a fn-level allow).
#![allow(clippy::too_many_lines)]

use crate::app::Chat;
use crate::project_rail::ProjectRail;
use crate::rpc::start_job;
use leptos::prelude::*;
use serde_json::json;

const THREAD_TYPEAHEAD_RESET_MS: f64 = 900.0;

fn thread_icon() -> impl IntoView {
    view! {
        <svg class="nav-svg" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M21 15a4 4 0 0 1-4 4H8l-5 3V7a4 4 0 0 1 4-4h10a4 4 0 0 1 4 4z" />
        </svg>
    }
}

fn chat_icon() -> impl IntoView {
    view! {
        <svg class="nav-svg" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M7 8h10" />
            <path d="M7 12h6" />
            <path d="M5 19a3 3 0 0 1-3-3V7a3 3 0 0 1 3-3h14a3 3 0 0 1 3 3v9a3 3 0 0 1-3 3H9l-4 3z" />
        </svg>
    }
}

fn automation_icon() -> impl IntoView {
    view! {
        <svg class="nav-svg" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M13 2 4 14h7l-1 8 10-13h-7z" />
        </svg>
    }
}

fn skill_icon() -> impl IntoView {
    view! {
        <svg class="nav-svg" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M12 3v18" />
            <path d="M5 8h14" />
            <path d="M7 16h10" />
            <path d="M4 12h16" />
        </svg>
    }
}

fn wechat_icon() -> impl IntoView {
    view! {
        <svg class="nav-svg" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M9 4a6 6 0 0 0-6 6c0 2 1 3.7 2.7 4.8L5 18l3-1.4A8 8 0 0 0 9 16" />
            <path d="M14 8a6 6 0 0 1 6 6c0 1.7-.8 3.3-2.2 4.4L19 21l-2.6-1.2A7 7 0 0 1 14 20a6 6 0 0 1 0-12z" />
        </svg>
    }
}

fn settings_icon() -> impl IntoView {
    view! {
        <svg class="nav-svg" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8z" />
            <path d="M3 12h2" />
            <path d="M19 12h2" />
            <path d="M12 3v2" />
            <path d="M12 19v2" />
        </svg>
    }
}

/// The runtime command that tears down a chat's live backend session: an
/// own-engine `session` thread uses `session.close`; an external CLI (delegate)
/// thread uses `delegate.close`. Using the wrong one leaves the session running
/// on the daemon.
pub(crate) fn close_command_kind(backend: &str) -> &'static str {
    if backend == "session" {
        "session.close"
    } else {
        "delegate.close"
    }
}

/// Compact relative time from an epoch-ms timestamp: now / 3m / 2h / 4d / 1w.
pub(crate) fn rel_time(ms: f64) -> String {
    let secs = ((js_sys::Date::now() - ms).max(0.0)) / 1000.0;
    if secs < 60.0 {
        return "now".into();
    }
    let mins = secs / 60.0;
    if mins < 60.0 {
        return format!("{}m", mins as u64);
    }
    let hours = mins / 60.0;
    if hours < 24.0 {
        return format!("{}h", hours as u64);
    }
    let days = hours / 24.0;
    if days < 7.0 {
        return format!("{}d", days as u64);
    }
    format!("{}w", (days / 7.0) as u64)
}

fn thread_agent_badge(agent: &str, live: bool) -> String {
    let label = crate::data::agent_label(agent);
    if live {
        format!("{label} · live")
    } else {
        label.to_string()
    }
}

fn thread_agent_class(live: bool) -> &'static str {
    if live {
        "rail-backend delegate live"
    } else {
        "rail-backend delegate"
    }
}

#[derive(Clone, Default)]
struct ThreadTypeaheadState {
    text: String,
    at_ms: f64,
}

#[derive(Clone)]
struct ThreadRow {
    index: usize,
    title: String,
    session: Option<String>,
    agent: String,
    updated_ms: f64,
}

fn visible_thread_rows(chats: &[Chat], query: &str, active: usize) -> Vec<ThreadRow> {
    let q = query.trim().to_lowercase();
    let mut rows: Vec<ThreadRow> = chats
        .iter()
        .enumerate()
        .filter(|(i, c)| q.is_empty() || *i == active || c.title.to_lowercase().contains(&q))
        .map(|(index, chat)| ThreadRow {
            index,
            title: chat.title.clone(),
            session: chat.session.clone(),
            agent: chat.agent.clone(),
            updated_ms: chat.updated_ms,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.updated_ms
            .partial_cmp(&a.updated_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn thread_key_target(rows: &[ThreadRow], current: usize, key: &str) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let pos = rows
        .iter()
        .position(|row| row.index == current)
        .unwrap_or(0);
    let target = match key {
        "ArrowDown" => (pos + 1).min(rows.len() - 1),
        "ArrowUp" => pos.saturating_sub(1),
        "Home" => 0,
        "End" => rows.len() - 1,
        _ => return None,
    };
    Some(rows[target].index)
}

fn thread_typeahead_target(
    rows: &[ThreadRow],
    current: usize,
    state: &mut ThreadTypeaheadState,
    key: &str,
    now_ms: f64,
) -> Option<usize> {
    let ch = printable_thread_char(key)?;
    let continuing = !state.text.is_empty() && now_ms - state.at_ms <= THREAD_TYPEAHEAD_RESET_MS;
    if !continuing {
        state.text.clear();
    }
    state.at_ms = now_ms;
    state.text.push(ch.to_ascii_lowercase());
    let start = thread_search_start(rows, current, continuing);
    if let Some(index) = find_thread_match(rows, start, &state.text) {
        return Some(index);
    }
    state.text.clear();
    state.text.push(ch.to_ascii_lowercase());
    find_thread_match(rows, thread_search_start(rows, current, false), &state.text)
}

fn printable_thread_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() || ch.is_whitespace() {
        return None;
    }
    Some(ch)
}

fn thread_search_start(rows: &[ThreadRow], current: usize, continuing: bool) -> usize {
    let pos = rows
        .iter()
        .position(|row| row.index == current)
        .unwrap_or(0);
    if continuing {
        pos
    } else {
        pos.saturating_add(1)
    }
}

fn find_thread_match(rows: &[ThreadRow], start: usize, needle: &str) -> Option<usize> {
    if rows.is_empty() || needle.is_empty() {
        return None;
    }
    (0..rows.len())
        .map(|offset| (start + offset) % rows.len())
        .find(|&pos| rows[pos].title.to_lowercase().starts_with(needle))
        .map(|pos| rows[pos].index)
}

#[component]
pub(crate) fn Sidebar(
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    input: RwSignal<String>,
    error: RwSignal<Option<String>>,
    token: StoredValue<Option<String>>,
    search: RwSignal<String>,
    workspace: RwSignal<String>,
    workspaces: RwSignal<Vec<(String, String)>>,
    settings_open: RwSignal<bool>,
    mode: RwSignal<&'static str>,
    inspector_open: RwSignal<bool>,
    inspector_tab: RwSignal<&'static str>,
    open_inspector_tab: Callback<&'static str>,
    chat_backend: RwSignal<String>,
    wechat_open: RwSignal<bool>,
    native_file_dialogs: Signal<bool>,
    branch: RwSignal<String>,
    branch_loading: RwSignal<bool>,
    reveal_workspace: Callback<()>,
    protocol_version: RwSignal<Option<String>>,
    busy: Signal<bool>,
) -> impl IntoView {
    let thread_typeahead = RwSignal::new(ThreadTypeaheadState::default());

    let focus_active_thread = move || {
        let query = search.get_untracked();
        let cur = active.get_untracked();
        let rows = chats.with_untracked(|all| visible_thread_rows(all, &query, cur));
        if let Some(row) = rows
            .iter()
            .find(|row| row.index == cur)
            .or_else(|| rows.first())
        {
            active.set(row.index);
            mode.set("chat");
            crate::dom::focus_thread_row(row.index);
        }
    };

    let focus_thread = move |index: usize| {
        active.set(index);
        mode.set("chat");
        crate::dom::focus_thread_row(index);
    };

    let thread_typeahead_jump = move |current: usize, key: String| -> bool {
        let query = search.get_untracked();
        let cur = active.get_untracked();
        let rows = chats.with_untracked(|all| visible_thread_rows(all, &query, cur));
        if rows.is_empty() {
            return false;
        }
        let now_ms = js_sys::Date::now();
        let mut target = None;
        thread_typeahead.update(|state| {
            target = thread_typeahead_target(&rows, current, state, &key, now_ms);
        });
        if let Some(index) = target {
            focus_thread(index);
            return true;
        }
        false
    };

    Effect::new(move |_| {
        let _ = search.get();
        thread_typeahead.set(ThreadTypeaheadState::default());
    });

    let new_chat = move |_| {
        let mut idx = 0;
        let backend = chat_backend.get_untracked();
        chats.update(|cs| {
            cs.push(Chat::new_with_backend(backend));
            idx = cs.len() - 1;
        });
        active.set(idx);
        input.set(String::new());
        error.set(None);
        search.set(String::new());
        mode.set("chat");
        crate::dom::focus_message_input();
    };

    // Close a chat: end its live backend session (best effort), remove it, fix
    // `active`. Keeps at least one.
    let close_chat = move |idx: usize, session: Option<String>| {
        let kind = close_command_kind(
            &chats
                .with_untracked(|cs| cs.get(idx).map(|c| c.backend.clone()))
                .unwrap_or_default(),
        );
        if let (Some(tok), Some(sid)) = (token.get_value(), session) {
            leptos::task::spawn_local(async move {
                let _ = start_job(&tok, json!({ "kind": kind, "session_id": sid })).await;
            });
        }
        let new_backend = chat_backend.get_untracked();
        chats.update(|cs| {
            if cs.len() > 1 {
                cs.remove(idx);
            } else {
                cs[0] = Chat::new_with_backend(new_backend);
            }
        });
        let len = chats.with_untracked(|cs| cs.len());
        active.update(|a| {
            if idx < *a {
                *a = a.saturating_sub(1);
            }
            if *a >= len {
                *a = len.saturating_sub(1);
            }
        });
        mode.set("chat");
        crate::dom::focus_thread_row(active.get_untracked());
    };

    view! {
        <aside id="sidebar-panel" class="sidebar">
            <div class="brand"><span class="spark">"N"</span><span>"Nerve"</span></div>
            <button type="button" class="newchat" title="New thread" aria-label="Create new thread" aria-keyshortcuts="Meta+N Control+N" on:click=new_chat>
                <span class="plus">"+"</span>"New thread"
            </button>
            <div class="sidebar-search" class:has-text=move || !search.get().is_empty()>
                <span class="search-ic">"⌕"</span>
                <input id="thread-search" class="search-in" type="search" placeholder="Search threads"
                    spellcheck="false"
                    aria-label="Search threads. Press Down Arrow to enter the thread list."
                    aria-controls="thread-list"
                    aria-keyshortcuts="Meta+F Control+F"
                    prop:value=move || search.get()
                    on:input=move |ev| search.set(event_target_value(&ev))
                    on:keydown=move |ev| match ev.key().as_str() {
                        "ArrowDown" => {
                            ev.prevent_default();
                            focus_active_thread();
                        }
                        "Escape" if !search.get_untracked().is_empty() => {
                            ev.prevent_default();
                            search.set(String::new());
                            thread_typeahead.set(ThreadTypeaheadState::default());
                        }
                        _ => {}
                    } />
                <button type="button" class="search-clear" title="Clear" aria-label="Clear thread search"
                    hidden=move || search.get().is_empty()
                    on:click=move |_| {
                        search.set(String::new());
                        crate::dom::focus_thread_search();
                    }>"×"</button>
            </div>
            <nav class="nav" aria-label="Workspace navigation">
                <button type="button"
                    class="nav-row"
                    class:on=move || mode.get() == "chat"
                    title="Threads"
                    aria-current=move || if mode.get() == "chat" { "page" } else { "false" }
                    aria-controls="surface-chat"
                    aria-keyshortcuts="Meta+1 Control+1"
                    on:click=move |_| {
                        mode.set("chat");
                        crate::dom::focus_message_input();
                    }
                >
                    <span class="nav-icon">{thread_icon()}</span><span>"Threads"</span>
                </button>
                <button type="button"
                    class="nav-row"
                    class:on=move || mode.get() == "context"
                    title="Context"
                    aria-current=move || if mode.get() == "context" { "page" } else { "false" }
                    aria-controls="surface-context"
                    aria-keyshortcuts="Meta+2 Control+2"
                    on:click=move |_| {
                        mode.set("context");
                        crate::dom::focus_context_filter();
                    }
                >
                    <span class="nav-icon">{chat_icon()}</span><span>"Context"</span>
                </button>
                <button type="button"
                    class="nav-row"
                    class:on=move || inspector_open.get() && inspector_tab.get() == "review"
                    title="Review packet"
                    aria-controls="inspector-panel review-panel"
                    aria-expanded=move || (inspector_open.get() && inspector_tab.get() == "review").to_string()
                    aria-keyshortcuts="Meta+3 Control+3"
                    on:click=move |_| open_inspector_tab.run("review")
                >
                    <span class="nav-icon">{automation_icon()}</span><span>"Review"</span>
                </button>
                <button type="button"
                    class="nav-row"
                    class:on=move || inspector_open.get() && inspector_tab.get() == "plan"
                    title="Tool activity"
                    aria-controls="inspector-panel tool-panel"
                    aria-expanded=move || (inspector_open.get() && inspector_tab.get() == "plan").to_string()
                    aria-keyshortcuts="Meta+4 Control+4"
                    on:click=move |_| open_inspector_tab.run("plan")
                >
                    <span class="nav-icon">{skill_icon()}</span><span>"Tools"</span>
                </button>
                <button type="button"
                    class="nav-row"
                    class:on=move || wechat_open.get()
                    title="WeChat bridge"
                    aria-controls="wechat-dialog"
                    aria-expanded=move || wechat_open.get().to_string()
                    on:click=move |_| wechat_open.set(true)
                >
                    <span class="nav-icon">{wechat_icon()}</span><span>"WeChat"</span>
                </button>
            </nav>
            <ProjectRail token=token workspace=workspace workspaces=workspaces native_file_dialogs=native_file_dialogs
                branch=branch branch_loading=branch_loading reveal=reveal_workspace/>
            <div class="thread-rail-wrap">
                <div class="rail-label">"Threads"</div>
                <div id="thread-list" class="rail rail-nested" role="list" aria-label="Threads">
                    {move || {
                        let cur = active.get();
                        let query = search.get();
                        // Keep ORIGINAL Vec indices (active/close index the full Vec).
                        let rows = chats.with(|all| visible_thread_rows(all, &query, cur));
                        if rows.is_empty() {
                            return view! { <div class="rail-empty" role="status">"No matches"</div> }.into_any();
                        }
                        let row_count = rows.len();
                        rows.into_iter().enumerate().map(|(visible_index, row)| {
                            let i = row.index;
                            let cls = if i == cur { "rail-row on" } else { "rail-row" };
                            let live = row.session.is_some();
                            let agent_badge = thread_agent_badge(&row.agent, live);
                            let agent_class = thread_agent_class(live);
                            let title = row.title;
                            let stamp = rel_time(row.updated_ms);
                            let session_for_key = row.session.clone();
                            let session_for_close = row.session.clone();
                            let thread_label = if i == cur {
                                format!("Current thread: {title}, {agent_badge}, updated {stamp}")
                            } else {
                                format!("Thread: {title}, {agent_badge}, updated {stamp}")
                            };
                            let close_label = format!("Close thread: {title}");
                            let pos = (visible_index + 1).to_string();
                            let size = row_count.to_string();
                            view! {
                                <div class=cls role="listitem" aria-posinset=pos aria-setsize=size>
                                    <button type="button" id=format!("thread-row-{i}") class="rail-pick thread-pick"
                                        tabindex=if i == cur { "0" } else { "-1" }
                                        aria-current=if i == cur { "true" } else { "false" }
                                        aria-label=thread_label
                                        aria-keyshortcuts="ArrowUp ArrowDown Home End Delete Backspace"
                                        on:keydown=move |ev| {
                                            if ev.key() == "Escape" {
                                                ev.prevent_default();
                                                crate::dom::focus_thread_search();
                                                return;
                                            }
                                            let query = search.get_untracked();
                                            let cur = active.get_untracked();
                                            let rows = chats.with_untracked(|all| visible_thread_rows(all, &query, cur));
                                            if let Some(next) = thread_key_target(&rows, i, &ev.key()) {
                                                ev.prevent_default();
                                                focus_thread(next);
                                                return;
                                            }
                                            if ev.key() == "Delete" || ev.key() == "Backspace" {
                                                ev.prevent_default();
                                                close_chat(i, session_for_key.clone());
                                                return;
                                            }
                                            if !(ev.meta_key() || ev.ctrl_key() || ev.alt_key())
                                                && thread_typeahead_jump(i, ev.key())
                                            {
                                                ev.prevent_default();
                                            }
                                        }
                                        on:click=move |_| focus_thread(i)>
                                        <span class="rail-dot" class:live=live></span>
                                        <span class="rail-copy">
                                            <span class="rail-title">{title}</span>
                                            <span class="rail-meta">
                                                <span class="rail-sub">{stamp}</span>
                                                <span class=agent_class>{agent_badge}</span>
                                            </span>
                                        </span>
                                    </button>
                                    <button type="button" class="rail-close" title="Close thread" aria-label=close_label tabindex="-1"
                                        on:click=move |_| close_chat(i, session_for_close.clone())>"×"</button>
                                </div>
                            }
                        }).collect_view().into_any()
                    }}
                </div>
            </div>
            <div class="spacer"></div>
            <button type="button" class="nav-row settings-row" title="Settings" aria-label="Open settings" aria-keyshortcuts="Meta+, Control+," on:click=move |_| settings_open.set(true)>
                <span class="nav-icon">{settings_icon()}</span><span>"Settings"</span>
            </button>
            <div class="status-row" role="status" aria-live="polite" aria-label=move || if busy.get() { "Runtime running" } else { "Runtime ready" }>
                {move || if busy.get() {
                    view! { <span class="dot busy" aria-hidden="true"></span>"running" }.into_any()
                } else {
                    // Read the live protocol version from `runtime/info` instead of a
                    // hardcoded number; fall back to a neutral label until it loads.
                    let label = protocol_version
                        .get()
                        .map_or_else(|| "runtime".to_string(), |v| format!("runtime v{v}"));
                    view! { <span class="dot idle" aria-hidden="true"></span>{label} }.into_any()
                }}
            </div>
        </aside>
    }
}

#[cfg(test)]
mod tests {
    use super::{close_command_kind, thread_agent_badge, thread_agent_class};

    #[test]
    fn close_command_matches_the_backend() {
        assert_eq!(close_command_kind("session"), "session.close");
        assert_eq!(close_command_kind("delegate"), "delegate.close");
        assert_eq!(close_command_kind("claude"), "delegate.close");
        assert_eq!(close_command_kind(""), "delegate.close");
    }

    #[test]
    fn thread_agent_badge_marks_live_state() {
        assert_eq!(thread_agent_badge("claude", true), "Claude Code · live");
        assert_eq!(thread_agent_badge("claude", false), "Claude Code");
        assert_eq!(thread_agent_badge("codex", false), "Codex");
    }

    #[test]
    fn thread_agent_class_marks_live_sessions() {
        assert_eq!(thread_agent_class(true), "rail-backend delegate live");
        assert_eq!(thread_agent_class(false), "rail-backend delegate");
    }
}
