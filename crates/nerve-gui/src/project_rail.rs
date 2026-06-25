//! Project/workspace rail: add/remove/switch workspaces with native-list keyboard
//! behavior, plus per-row git-branch context and an OS reveal affordance.

// The `ProjectRail` `#[component]` expands to one declarative view tree; the
// `#[component]` macro does not forward a fn-level allow, so the workspace-wide
// `too_many_lines` deny is applied at module scope (matching app.rs / sidebar.rs).
#![allow(clippy::too_many_lines)]

use leptos::prelude::*;

const PROJECT_TYPEAHEAD_RESET_MS: f64 = 900.0;
const NO_TOKEN: &str = "No daemon token — open the daemon URL or append #token=…";

#[derive(Clone, Default)]
struct ProjectTypeaheadState {
    text: String,
    at_ms: f64,
}

/// Signals threaded into the project rows: the active workspace's git branch, a
/// loading flag (drives the switch-time skeleton), and the OS reveal callback
/// (reuses `workspace.reveal` — no new protocol).
#[derive(Clone, Copy)]
struct RailSignals {
    branch: RwSignal<String>,
    branch_loading: RwSignal<bool>,
    reveal: Callback<()>,
}

#[component]
pub(crate) fn ProjectRail(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    workspaces: RwSignal<Vec<(String, String)>>,
    native_file_dialogs: Signal<bool>,
    branch: RwSignal<String>,
    branch_loading: RwSignal<bool>,
    reveal: Callback<()>,
) -> impl IntoView {
    let signals = RailSignals {
        branch,
        branch_loading,
        reveal,
    };
    let adding = RwSignal::new(false);
    let new_path = RwSignal::new(String::new());
    let pick_status = RwSignal::new(String::new());
    let picking = RwSignal::new(false);
    let typeahead = RwSignal::new(ProjectTypeaheadState::default());

    let focus_project = move |index: usize, name: String| {
        workspace.set(name);
        crate::dom::focus_project_row(index);
    };

    let do_add = move || add_project(token, workspace, workspaces, new_path, adding);
    let do_remove = move |name: String| remove_project(token, workspace, workspaces, name);

    Effect::new(move |_| normalize_workspace(workspace, &workspaces.get()));
    Effect::new(move |_| {
        let _ = workspaces.get();
        typeahead.set(ProjectTypeaheadState::default());
    });

    let jump_project = move |current: usize, key: String| -> bool {
        let list = workspaces.get_untracked();
        if list.is_empty() {
            return false;
        }
        let now_ms = js_sys::Date::now();
        let mut target = None;
        typeahead
            .update(|state| target = project_typeahead_target(&list, current, state, &key, now_ms));
        if let Some(index) = target
            && let Some((name, _)) = list.get(index)
        {
            focus_project(index, name.clone());
            return true;
        }
        false
    };

    view! {
        <>
            <div class="rail-label rail-label-row">
                <span>"Projects"</span>
                <div class="rail-label-actions">
                    <button type="button" class="rail-reveal" title="Reveal active project in file manager"
                        aria-label="Reveal active project in file manager"
                        disabled=move || workspace.get().is_empty()
                        on:click=move |_| reveal.run(())>
                        {reveal_icon()}
                    </button>
                    <button type="button" id="project-add-button" class="rail-add" title="Add project"
                        aria-label=move || if adding.get() { "Cancel adding project" } else { "Add project" }
                        aria-controls="project-add"
                        aria-expanded=move || adding.get().to_string()
                        on:click=move |_| {
                            let opening = !adding.get_untracked();
                            adding.set(opening);
                            if opening {
                                crate::dom::focus_element_by_id_next_frame("project-add");
                            }
                        }>"+"</button>
                </div>
            </div>
            {move || adding.get().then(|| view! {
                <div class="proj-add-row">
                    <input id="project-add" class="proj-add-in" placeholder="Absolute path to a repo…"
                        spellcheck="false"
                        aria-label="Absolute path to a repo"
                        aria-describedby="project-add-help project-add-status"
                        aria-keyshortcuts="Enter Escape"
                        prop:value=move || new_path.get()
                        on:input=move |ev| {
                            pick_status.set(String::new());
                            new_path.set(event_target_value(&ev));
                        }
                        on:keydown=move |ev| match ev.key().as_str() {
                            "Enter" => { ev.prevent_default(); do_add(); }
                            "Escape" => {
                                ev.prevent_default();
                                adding.set(false);
                                crate::dom::focus_element_by_id_next_frame("project-add-button");
                            }
                            _ => {}
                        } />
                    {move || native_file_dialogs.get().then(|| view! {
                        <button type="button" class="proj-add-choose" disabled=move || picking.get()
                            aria-label="Choose project folder with native picker"
                            aria-controls="project-add project-add-status"
                            on:click=move |_| choose_project_folder(token, new_path, pick_status, picking)>
                            "Choose…"
                        </button>
                    })}
                </div>
                <span id="project-add-help" class="sr-only">"Press Enter to add this project, or Escape to cancel."</span>
                <span id="project-add-status" class="proj-add-status" role="status" aria-live="polite" aria-busy=move || picking.get().to_string()>
                    {move || pick_status.get()}
                </span>
            })}
            <div class="proj-list" role="list" aria-label="Projects">
                {move || project_rows(workspaces.get(), workspace.get(), focus_project, do_remove, jump_project, signals)}
            </div>
        </>
    }
}

fn normalize_workspace(workspace: RwSignal<String>, list: &[(String, String)]) {
    let current = workspace.get_untracked();
    if list.is_empty() {
        if !current.is_empty() {
            workspace.set(String::new());
        }
        return;
    }
    if !list.iter().any(|(name, _)| name == &current)
        && let Some((first, _)) = list.first()
    {
        workspace.set(first.clone());
    }
}

fn choose_project_folder(
    token: StoredValue<Option<String>>,
    new_path: RwSignal<String>,
    status: RwSignal<String>,
    picking: RwSignal<bool>,
) {
    let Some(tok) = token.get_value() else {
        status.set(NO_TOKEN.into());
        return;
    };
    if picking.get_untracked() {
        return;
    }
    picking.set(true);
    status.set("Opening folder picker…".into());
    leptos::task::spawn_local(async move {
        match crate::data::pick_host_folder(&tok, "Choose project folder").await {
            Ok(path) => {
                new_path.set(path);
                status.set("Folder selected.".into());
                crate::dom::focus_element_by_id_next_frame("project-add");
            }
            Err(err) if err.to_ascii_lowercase().contains("cancel") => {
                status.set("Folder selection cancelled.".into());
            }
            Err(err) => status.set(format!("Folder picker failed: {err}")),
        }
        picking.set(false);
    });
}

fn add_project(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    workspaces: RwSignal<Vec<(String, String)>>,
    new_path: RwSignal<String>,
    adding: RwSignal<bool>,
) {
    let Some(tok) = token.get_value() else { return };
    let path = new_path.get_untracked().trim().to_string();
    if path.is_empty() {
        return;
    }
    let name = project_name_from_path(&path);
    new_path.set(String::new());
    adding.set(false);
    leptos::task::spawn_local(async move {
        let list = crate::data::add_workspace(&tok, &name, &path).await;
        let focus_index = list
            .iter()
            .position(|(item, _)| item == &name)
            .unwrap_or_default();
        workspaces.set(list);
        workspace.set(name);
        crate::dom::focus_project_row(focus_index);
    });
}

pub(crate) fn project_name_from_path(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let source = if trimmed.is_empty() { path } else { trimmed };
    source
        .rsplit(['/', '\\'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(source)
        .to_string()
}

fn remove_project(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    workspaces: RwSignal<Vec<(String, String)>>,
    name: String,
) {
    if workspaces.with_untracked(|all| all.len()) <= 1 {
        return;
    }
    let Some(tok) = token.get_value() else { return };
    leptos::task::spawn_local(async move {
        let list = crate::data::remove_workspace(&tok, &name).await;
        let current = workspace.get_untracked();
        let next = if current == name || !list.iter().any(|(item, _)| item == &current) {
            list.first()
                .map(|(first, _)| first.clone())
                .unwrap_or_default()
        } else {
            current
        };
        let focus_index = list
            .iter()
            .position(|(item, _)| item == &next)
            .unwrap_or_default();
        workspace.set(next);
        let empty = list.is_empty();
        workspaces.set(list);
        if empty {
            crate::dom::focus_element_by_id_next_frame("project-add-button");
        } else {
            crate::dom::focus_project_row(focus_index);
        }
    });
}

/// Per-row data (non-closure) for [`project_row`]; bundled to keep the row
/// builder's argument count under the clippy gate.
struct ProjectRowArgs {
    index: usize,
    name: String,
    root: String,
    selected: bool,
    multi: bool,
    len: usize,
    names: Vec<String>,
    signals: RailSignals,
}

fn project_rows(
    list: Vec<(String, String)>,
    current: String,
    focus_project: impl Fn(usize, String) + Copy + 'static,
    do_remove: impl Fn(String) + Copy + 'static,
    jump_project: impl Fn(usize, String) -> bool + Copy + 'static,
    signals: RailSignals,
) -> AnyView {
    if list.is_empty() {
        return view! {
            <div class="project-row project-row-empty" role="listitem" aria-label="No projects added">
                <span class="project-dot"></span>
                <span role="status">"No projects"</span>
            </div>
        }
        .into_any();
    }
    let multi = list.len() > 1;
    let len = list.len();
    let active_index = list
        .iter()
        .position(|(name, _)| name == &current)
        .unwrap_or_default();
    let names = list
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    list.into_iter()
        .enumerate()
        .map(|(index, (name, root))| {
            project_row(
                ProjectRowArgs {
                    index,
                    name,
                    root,
                    selected: index == active_index,
                    multi,
                    len,
                    names: names.clone(),
                    signals,
                },
                focus_project,
                do_remove,
                jump_project,
            )
        })
        .collect_view()
        .into_any()
}

fn project_row(
    args: ProjectRowArgs,
    focus_project: impl Fn(usize, String) + Copy + 'static,
    do_remove: impl Fn(String) + Copy + 'static,
    jump_project: impl Fn(usize, String) -> bool + Copy + 'static,
) -> AnyView {
    let ProjectRowArgs {
        index,
        name,
        root,
        selected,
        multi,
        len,
        names,
        signals,
    } = args;
    let pick = name.clone();
    let rm_for_key = name.clone();
    let rm_for_close = name.clone();
    let project_label = if selected {
        format!("Current project: {name}")
    } else {
        format!("Project: {name}")
    };
    let remove_label = format!("Remove project: {name}");
    view! {
        <div class="rail-row" class:on=selected role="listitem"
            aria-posinset=(index + 1).to_string() aria-setsize=len.to_string()>
            <button type="button" id=format!("project-row-{index}") class="rail-pick"
                title=root.clone()
                aria-current=if selected { "true" } else { "false" }
                aria-label=project_label
                aria-keyshortcuts="ArrowUp ArrowDown Home End Delete Backspace"
                tabindex=if selected { "0" } else { "-1" }
                on:keydown=move |ev| {
                    if ev.key() == "Escape" {
                        ev.prevent_default();
                        crate::dom::focus_element_by_id_next_frame("project-add-button");
                        return;
                    }
                    if let Some(next) = project_key_target(index, len, &ev.key()) {
                        ev.prevent_default();
                        if let Some(name) = names.get(next) {
                            focus_project(next, name.clone());
                        }
                        return;
                    }
                    if multi && (ev.key() == "Delete" || ev.key() == "Backspace") {
                        ev.prevent_default();
                        do_remove(rm_for_key.clone());
                        return;
                    }
                    if !(ev.meta_key() || ev.ctrl_key() || ev.alt_key()) && jump_project(index, ev.key()) {
                        ev.prevent_default();
                    }
                }
                on:click=move |_| focus_project(index, pick.clone())>
                <span class="project-dot" class:on=selected></span>
                <span class="rail-copy">
                    <span class="rail-title">{name}</span>
                    {selected.then(|| project_meta(signals))}
                </span>
            </button>
            {selected.then(|| view! {
                <button type="button" class="rail-reveal rail-reveal-row" tabindex="-1"
                    title="Reveal in file manager" aria-label="Reveal active project in file manager"
                    on:click=move |_| signals.reveal.run(())>{reveal_icon()}</button>
            })}
            {multi.then(|| view! {
                <button type="button" class="rail-close" title="Remove project" aria-label=remove_label tabindex="-1"
                    on:click=move |_| do_remove(rm_for_close.clone())>"×"</button>
            })}
        </div>
    }
    .into_any()
}

/// The active row's secondary line: just the git-branch chip (a skeleton while the
/// branch Effect resolves). The row shows folder **name** (the title) + **branch** —
/// not the workspace path, which stays available on hover via the row tooltip
/// (`title=root` on the rail-pick button).
fn project_meta(signals: RailSignals) -> impl IntoView {
    view! {
        <span class="proj-meta">
            {move || if signals.branch_loading.get() {
                view! {
                    <span class="proj-skel" aria-hidden="true"></span>
                    <span class="sr-only" role="status">"Loading branch…"</span>
                }.into_any()
            } else {
                let branch = signals.branch.get();
                let title = format!("Git branch: {branch}");
                view! {
                    <span class="proj-branch" title=title>
                        {git_icon()}<span class="proj-branch-name">{branch}</span>
                    </span>
                }.into_any()
            }}
        </span>
    }
}

fn reveal_icon() -> impl IntoView {
    view! {
        <svg class="rail-ic" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" />
        </svg>
    }
}

fn git_icon() -> impl IntoView {
    view! {
        <svg class="proj-branch-ic" viewBox="0 0 24 24" aria-hidden="true" focusable="false">
            <circle cx="6" cy="6" r="2.3" />
            <circle cx="6" cy="18" r="2.3" />
            <circle cx="18" cy="8" r="2.3" />
            <path d="M6 8.3v7.4" />
            <path d="M18 10.3a6 6 0 0 1-6 6H8.3" />
        </svg>
    }
}

fn project_key_target(current: usize, len: usize, key: &str) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match key {
        "ArrowDown" => Some((current + 1).min(len - 1)),
        "ArrowUp" => Some(current.saturating_sub(1)),
        "Home" => Some(0),
        "End" => Some(len - 1),
        _ => None,
    }
}

fn project_typeahead_target(
    list: &[(String, String)],
    current: usize,
    state: &mut ProjectTypeaheadState,
    key: &str,
    now_ms: f64,
) -> Option<usize> {
    let ch = printable_project_char(key)?;
    let continuing = !state.text.is_empty() && now_ms - state.at_ms <= PROJECT_TYPEAHEAD_RESET_MS;
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
    if let Some(index) = find_project_match(list, start, &state.text) {
        return Some(index);
    }
    state.text.clear();
    state.text.push(ch.to_ascii_lowercase());
    find_project_match(list, current.saturating_add(1), &state.text)
}

fn printable_project_char(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || ch.is_control() || ch.is_whitespace() {
        return None;
    }
    Some(ch)
}

fn find_project_match(list: &[(String, String)], start: usize, needle: &str) -> Option<usize> {
    if list.is_empty() || needle.is_empty() {
        return None;
    }
    (0..list.len())
        .map(|offset| (start + offset) % list.len())
        .find(|&index| list[index].0.to_lowercase().starts_with(needle))
}
