//! The sidebar: brand, New thread, a conversation search box, the project row,
//! the searchable + recency-sorted + timestamped conversation rail, and a footer
//! with Settings + runtime status. Owns new-thread / close-thread (sidebar
//! actions). Split out of `app.rs` to stay under the file-size gate.

// View-heavy component; see app.rs for why too_many_lines is allowed at module
// scope (the `#[component]` macro drops a fn-level allow).
#![allow(clippy::too_many_lines)]

use crate::app::Chat;
use crate::rpc::start_job;
use leptos::prelude::*;
use serde_json::json;

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
    busy: Signal<bool>,
) -> impl IntoView {
    let new_chat = move |_| {
        let mut idx = 0;
        chats.update(|cs| {
            cs.push(Chat::new());
            idx = cs.len() - 1;
        });
        active.set(idx);
        input.set(String::new());
        error.set(None);
        search.set(String::new());
    };

    // Close a chat: end its delegate session (best effort — else every thread
    // leaks a parked CLI child), remove it, fix `active`. Keeps at least one.
    let close_chat = move |idx: usize, session: Option<String>| {
        if let (Some(tok), Some(sid)) = (token.get_value(), session) {
            leptos::task::spawn_local(async move {
                let _ = start_job(&tok, json!({"kind": "delegate.close", "session_id": sid})).await;
            });
        }
        chats.update(|cs| {
            if cs.len() > 1 {
                cs.remove(idx);
            } else {
                cs[0] = Chat::new();
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
    };

    // Project management. `adding` toggles a path input; do_add registers a root
    // (name = its basename) and switches to it; do_remove drops one (keeping the
    // active selection valid, and never the last workspace).
    let adding = RwSignal::new(false);
    let new_path = RwSignal::new(String::new());
    let do_add = move || {
        let Some(tok) = token.get_value() else { return };
        let path = new_path.get_untracked().trim().to_string();
        if path.is_empty() {
            return;
        }
        let name = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(path.as_str())
            .to_string();
        new_path.set(String::new());
        adding.set(false);
        leptos::task::spawn_local(async move {
            let list = crate::data::add_workspace(&tok, &name, &path).await;
            workspaces.set(list);
            workspace.set(name);
        });
    };
    let do_remove = move |name: String| {
        if workspaces.with_untracked(|all| all.len()) <= 1 {
            return;
        }
        let Some(tok) = token.get_value() else { return };
        leptos::task::spawn_local(async move {
            let list = crate::data::remove_workspace(&tok, &name).await;
            if workspace.get_untracked() == name
                && let Some((first, _)) = list.first()
            {
                workspace.set(first.clone());
            }
            workspaces.set(list);
        });
    };

    view! {
        <aside class="sidebar">
            <div class="brand"><span class="spark">"N"</span><span>"Nerve"</span></div>
            <button class="newchat" title="New thread" on:click=new_chat>
                <span class="plus">"+"</span>"New thread"
            </button>
            <div class="sidebar-search" class:has-text=move || !search.get().is_empty()>
                <span class="search-ic">"⌕"</span>
                <input class="search-in" type="search" placeholder="Search threads"
                    prop:value=move || search.get()
                    on:input=move |ev| search.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Escape" && !search.get_untracked().is_empty() {
                            ev.prevent_default();
                            search.set(String::new());
                        }
                    } />
                <button class="search-clear" title="Clear" on:click=move |_| search.set(String::new())>"×"</button>
            </div>
            <div class="rail-label rail-label-row">
                <span>"Projects"</span>
                <button class="rail-add" title="Add project"
                    on:click=move |_| adding.update(|a| *a = !*a)>"+"</button>
            </div>
            {move || adding.get().then(|| view! {
                <input class="proj-add-in" placeholder="Absolute path to a repo…"
                    prop:value=move || new_path.get()
                    on:input=move |ev| new_path.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" { ev.prevent_default(); do_add(); }
                        if ev.key() == "Escape" { adding.set(false); }
                    } />
            })}
            <div class="proj-list">
                {move || {
                    let cur = workspace.get();
                    let list = workspaces.get();
                    if list.is_empty() {
                        return view! {
                            <div class="project-row"><span class="project-dot"></span>
                                <span>{move || workspace.get()}</span></div>
                        }.into_any();
                    }
                    let multi = list.len() > 1;
                    list.into_iter().map(|(name, _root)| {
                        let on = name == cur;
                        let pick = name.clone();
                        let rm = name.clone();
                        view! {
                            <div class="rail-row" class:on=on>
                                <button class="rail-pick" on:click=move |_| workspace.set(pick.clone())>
                                    <span class="project-dot"></span>
                                    <span class="rail-title">{name}</span>
                                </button>
                                {multi.then(|| {
                                    let rm = rm.clone();
                                    view! {
                                        <button class="rail-close" title="Remove project"
                                            on:click=move |_| do_remove(rm.clone())>"×"</button>
                                    }
                                })}
                            </div>
                        }
                    }).collect_view().into_any()
                }}
            </div>
            <div class="rail-label">"Conversations"</div>
            <div class="rail">
                {move || {
                    let q = search.get().trim().to_lowercase();
                    let cur = active.get();
                    // Keep ORIGINAL Vec indices (active/close index the full Vec):
                    // filter after enumerate, always keep the active chat, then sort
                    // a copy by recency so the latest thread floats to the top.
                    let mut rows: Vec<(usize, Chat)> = chats.get().into_iter().enumerate()
                        .filter(|(i, c)| q.is_empty() || *i == cur || c.title.to_lowercase().contains(&q))
                        .collect();
                    rows.sort_by(|a, b| b.1.updated_ms
                        .partial_cmp(&a.1.updated_ms).unwrap_or(std::cmp::Ordering::Equal));
                    if rows.is_empty() {
                        return view! { <div class="rail-empty">"No matches"</div> }.into_any();
                    }
                    rows.into_iter().map(|(i, c)| {
                        let cls = if i == cur { "rail-row on" } else { "rail-row" };
                        let live = c.session.is_some();
                        let title = c.title;
                        let stamp = rel_time(c.updated_ms);
                        let session = c.session.clone();
                        view! {
                            <div class=cls>
                                <button class="rail-pick" on:click=move |_| active.set(i)>
                                    <span class="rail-dot" class:live=live></span>
                                    <span class="rail-title">{title}</span>
                                    <span class="rail-time">{stamp}</span>
                                </button>
                                <button class="rail-close" title="Close thread"
                                    on:click=move |_| close_chat(i, session.clone())>"×"</button>
                            </div>
                        }
                    }).collect_view().into_any()
                }}
            </div>
            <div class="spacer"></div>
            <button class="nav-row settings-row" title="Settings" on:click=move |_| settings_open.set(true)>
                <span class="nav-icon">"⚙"</span><span>"Settings"</span>
            </button>
            <div class="status-row">
                {move || if busy.get() {
                    view! { <span class="dot busy"></span>"running" }.into_any()
                } else {
                    view! { <span class="dot idle"></span>"runtime v4" }.into_any()
                }}
            </div>
        </aside>
    }
}
