//! The Context view: pick files from a clickable tree, assemble a token-budgeted
//! context with a named recipe, then copy/export it — the RepoPrompt-style "build
//! context for an LLM" surface. All deterministic work lives in the engine
//! (`list_files` / `manage_selection` / `workspace_context` + recipes); this is a
//! thin client over `tool.call`.

// View-heavy component; see app.rs for why too_many_lines is allowed at module
// scope (the `#[component]` macro drops a fn-level allow).
#![allow(clippy::too_many_lines)]

use crate::clipboard::copy_text_with_note;
use crate::context_budget::{Budget, BudgetPanel};
use crate::context_manifest::{handoff_text, manifest_text};
use crate::context_selection::SelectionManifest;
use crate::context_view_support::{
    TypeaheadState, context_row_target, context_typeahead_target, visible_files,
    visible_matching_count,
};
use crate::data::{
    FileRow, SelectionSummary, fetch_context, fetch_diff, list_files, save_host_text_file,
    selection_op, selection_summary,
};
use leptos::prelude::*;

const RECIPES: &[(&str, &str)] = &[
    ("standard", "Standard"),
    ("plan", "Plan"),
    ("review", "Review"),
    ("diff", "Diff Follow-Up"),
    ("manual", "Manual"),
];

const FILE_LIMIT: usize = 1500;
#[component]
pub fn ContextView(
    token: StoredValue<Option<String>>,
    /// The active workspace name, threaded into every tool call so the Context
    /// surface follows the selected project.
    workspace: RwSignal<String>,
    /// Top-level surface mode; Escape from an empty filter returns to Chat.
    mode: RwSignal<&'static str>,
    native_file_dialogs: Signal<bool>,
) -> impl IntoView {
    let recipe = RwSignal::new("standard".to_string());
    let filter = RwSignal::new(String::new());
    let selected_only = RwSignal::new(false);
    let files = RwSignal::new(Vec::<FileRow>::new());
    let truncated = RwSignal::new(false);
    let context_text = RwSignal::new(String::new());
    let budget = RwSignal::new(Budget::default());
    let selection = RwSignal::new(SelectionSummary::default());
    let batch_note = RwSignal::new(String::new());
    let saving_handoff = RwSignal::new(false);
    let active_row = RwSignal::new(0usize);
    let typeahead = RwSignal::new(TypeaheadState::default());

    // Re-assemble the context: fetch the diff for recipes that need it, then render.
    let refresh = move || {
        let Some(tok) = token.get_value() else { return };
        let rec = recipe.get_untracked();
        let ws = workspace.get_untracked();
        leptos::task::spawn_local(async move {
            let git_diff = if rec == "review" || rec == "diff" {
                fetch_diff(&tok, &ws).await
            } else {
                None
            };
            if let Some((text, structured)) = fetch_context(&tok, &rec, git_diff, &ws).await {
                context_text.set(text);
                let parsed = structured
                    .get("tokens")
                    .cloned()
                    .and_then(|t| serde_json::from_value::<Budget>(t).ok())
                    .unwrap_or_default();
                budget.set(parsed);
            }
        });
    };

    let load_selection = move || {
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        leptos::task::spawn_local(async move {
            selection.set(selection_summary(&tok, &ws).await);
        });
    };

    // Reload the (selection-aware) file list for the current filter. A generation
    // counter drops stale, out-of-order responses (per-keystroke fetches racing).
    let load_gen = StoredValue::new(0u32);
    let load_files = move || {
        let Some(tok) = token.get_value() else { return };
        let query = filter.get_untracked();
        let ws = workspace.get_untracked();
        let generation = load_gen.get_value() + 1;
        load_gen.set_value(generation);
        leptos::task::spawn_local(async move {
            let (rows, trunc) = list_files(&tok, &query, FILE_LIMIT, &ws).await;
            if load_gen.get_value() == generation {
                files.set(rows);
                truncated.set(trunc);
            }
        });
    };

    // Re-run on recipe / filter changes AND when the active workspace switches.
    Effect::new(move |_| {
        let _ = (recipe.get(), workspace.get());
        refresh();
    });
    Effect::new(move |_| {
        let _ = (filter.get(), workspace.get());
        load_files();
    });
    Effect::new(move |_| {
        let _ = workspace.get();
        load_selection();
    });
    Effect::new(move |_| {
        let _ = (filter.get(), selected_only.get(), workspace.get());
        active_row.set(0);
        typeahead.set(TypeaheadState::default());
    });
    Effect::new(move |_| {
        let len = files.with(|rows| visible_files(rows, selected_only.get()).len());
        active_row.update(|index| {
            if len == 0 {
                *index = 0;
            } else if *index >= len {
                *index = len - 1;
            }
        });
    });

    // Toggle a file's selection, then refresh the list state + the budget.
    let toggle = move |path: String, selected: bool| {
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        let op = if selected { "remove" } else { "add" };
        leptos::task::spawn_local(async move {
            let _ = selection_op(&tok, op, vec![path], &ws).await;
            load_files();
            load_selection();
            refresh();
        });
    };

    let toggle_first_match = move || {
        if let Some(row) = files.with_untracked(|rows| {
            visible_files(rows, selected_only.get_untracked())
                .first()
                .cloned()
        }) {
            toggle(row.path, row.selected);
        }
    };

    let focus_active_row = move || {
        let len =
            files.with_untracked(|rows| visible_files(rows, selected_only.get_untracked()).len());
        if len == 0 {
            return;
        }
        let index = active_row.get_untracked().min(len - 1);
        active_row.set(index);
        crate::dom::focus_context_row(index);
    };

    let typeahead_jump = move |key: String| -> bool {
        let rows = files.with_untracked(|rows| visible_files(rows, selected_only.get_untracked()));
        if rows.is_empty() {
            return false;
        }
        let current = active_row.get_untracked().min(rows.len() - 1);
        let now_ms = js_sys::Date::now();
        let mut target = None;
        typeahead.update(|state| {
            target = context_typeahead_target(&rows, current, state, &key, now_ms);
        });
        if let Some(next) = target {
            active_row.set(next);
            crate::dom::focus_context_row(next);
            return true;
        }
        false
    };

    let can_select_shown =
        move || files.with(|rows| visible_matching_count(rows, selected_only.get(), false) > 0);
    let can_remove_shown =
        move || files.with(|rows| visible_matching_count(rows, selected_only.get(), true) > 0);
    let select_shown_label = move || {
        let count = files.with(|rows| visible_matching_count(rows, selected_only.get(), false));
        if count == 0 {
            "No shown files available to select".to_string()
        } else {
            format!("Select {count} shown files into context")
        }
    };
    let remove_shown_label = move || {
        let count = files.with(|rows| visible_matching_count(rows, selected_only.get(), true));
        if count == 0 {
            "No shown selected files to remove".to_string()
        } else {
            format!("Remove {count} shown files from context")
        }
    };
    let clear_selection_label = move || {
        let count = selection.with(|summary| summary.files.len());
        if count == 0 {
            "No context selection to clear".to_string()
        } else {
            format!("Clear context selection with {count} selected files")
        }
    };
    let recipe_select_label = move || format!("Context recipe: {}", recipe_label(&recipe.get()));

    let select_shown = Callback::new(move |_| {
        let paths = files.with_untracked(|rows| {
            visible_files(rows, selected_only.get_untracked())
                .into_iter()
                .filter(|row| !row.selected)
                .map(|row| row.path)
                .collect::<Vec<_>>()
        });
        if paths.is_empty() {
            return;
        }
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        batch_note.set(format!("Selecting {} shown…", paths.len()));
        leptos::task::spawn_local(async move {
            let _ = selection_op(&tok, "add", paths, &ws).await;
            batch_note.set("Selected shown files.".into());
            load_files();
            load_selection();
            refresh();
        });
    });

    let remove_shown = Callback::new(move |_| {
        let paths = files.with_untracked(|rows| {
            visible_files(rows, selected_only.get_untracked())
                .into_iter()
                .filter(|row| row.selected)
                .map(|row| row.path)
                .collect::<Vec<_>>()
        });
        if paths.is_empty() {
            return;
        }
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        batch_note.set(format!("Removing {} shown…", paths.len()));
        leptos::task::spawn_local(async move {
            let _ = selection_op(&tok, "remove", paths, &ws).await;
            batch_note.set("Removed shown files.".into());
            load_files();
            load_selection();
            refresh();
        });
    });

    let selection_stats = move || {
        let selected = selection.with(|summary| summary.files.len());
        let tokens = selection.with(|summary| summary.total_tokens);
        files.with(|rows| {
            let shown = visible_files(rows, selected_only.get()).len();
            format!("{selected} selected · {tokens} tokens · {shown} shown")
        })
    };

    let clear_selection = move |_| {
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        leptos::task::spawn_local(async move {
            let _ = selection_op(&tok, "clear", Vec::new(), &ws).await;
            load_files();
            load_selection();
            refresh();
        });
    };

    let copy = move |_| {
        let text = context_text.get_untracked();
        copy_text_with_note(text, batch_note, "Copied assembled context.");
    };

    let copy_manifest = Callback::new(move |_| {
        let text = selection.with_untracked(manifest_text);
        copy_text_with_note(text, batch_note, "Copied selection manifest.");
    });

    let remove_selected_file = Callback::new(move |path: String| {
        toggle(path, true);
    });

    let copy_handoff = move |_| {
        let summary = selection.get_untracked();
        let ws = workspace.get_untracked();
        let rec = recipe.get_untracked();
        let context = context_text.get_untracked();
        let text = handoff_text(&summary, &ws, &rec, &context);
        copy_text_with_note(text, batch_note, "Copied context handoff.");
    };

    let save_handoff = move |_| {
        if saving_handoff.get_untracked() {
            return;
        }
        let Some(tok) = token.get_value() else {
            batch_note.set("No daemon token; cannot save context handoff.".into());
            return;
        };
        let summary = selection.get_untracked();
        let ws = workspace.get_untracked();
        let rec = recipe.get_untracked();
        let context = context_text.get_untracked();
        let text = handoff_text(&summary, &ws, &rec, &context);
        let file_name = context_handoff_file_name(&ws, &rec);
        saving_handoff.set(true);
        batch_note.set("Opening save panel…".into());
        leptos::task::spawn_local(async move {
            let note = match save_host_text_file(&tok, "Save context handoff", &file_name, text)
                .await
            {
                Ok(path) => format!("Saved context handoff to {path}."),
                Err(err) if err.to_ascii_lowercase().contains("cancel") => "Save cancelled.".into(),
                Err(err) => format!("Save failed: {err}"),
            };
            batch_note.set(note);
            saving_handoff.set(false);
        });
    };

    let copy_handoff_shortcut = move |ev: leptos::ev::KeyboardEvent| {
        if (ev.meta_key() || ev.ctrl_key()) && ev.shift_key() && ev.key().eq_ignore_ascii_case("c")
        {
            ev.prevent_default();
            let summary = selection.get_untracked();
            let ws = workspace.get_untracked();
            let rec = recipe.get_untracked();
            let context = context_text.get_untracked();
            let text = handoff_text(&summary, &ws, &rec, &context);
            copy_text_with_note(text, batch_note, "Copied context handoff.");
        }
    };

    view! {
        <aside class="context-view" aria-keyshortcuts="Meta+Shift+C Control+Shift+C" on:keydown=copy_handoff_shortcut>
            <div class="ctx-head">
                <div class="ctx-title-block">
                    <span class="ctx-title">"Context"</span>
                    <span id="context-selection-summary" class="ctx-subtitle">{selection_stats}</span>
                    <span id="context-operation-status" class="ctx-op-note" role="status" aria-live="polite">
                        {move || batch_note.get()}
                    </span>
                </div>
                <div class="ctx-head-actions" role="group" aria-label="Context selection actions" aria-describedby="context-selection-summary">
                    <button type="button" class="ctx-copy ctx-batch" disabled=move || !can_select_shown()
                        aria-label=select_shown_label
                        aria-controls="context-file-list context-selection-manifest"
                        on:click=move |_| select_shown.run(())>"Select shown"</button>
                    <button type="button" class="ctx-copy ctx-batch" disabled=move || !can_remove_shown()
                        aria-label=remove_shown_label
                        aria-controls="context-file-list context-selection-manifest"
                        on:click=move |_| remove_shown.run(())>"Remove shown"</button>
                    <button type="button" class="ctx-copy ctx-clear" disabled=move || selection.with(|summary| summary.files.is_empty())
                        aria-label=clear_selection_label
                        aria-controls="context-file-list context-selection-manifest"
                        on:click=clear_selection>"Clear"</button>
                    <select class="ctx-recipe" aria-label=recipe_select_label title="Context recipe" prop:value=move || recipe.get()
                        on:change=move |ev| recipe.set(event_target_value(&ev))>
                        {RECIPES.iter().map(|(id, label)| view! { <option value=*id>{*label}</option> }).collect_view()}
                    </select>
                </div>
            </div>

            <div class="ctx-filter-row">
                <input id="context-filter" class="ctx-add-in" placeholder="filter files…  Enter toggles first match · Esc clears"
                    spellcheck="false"
                    aria-label="Filter files. Press Enter to toggle the first match, Escape to clear."
                    aria-controls="context-file-list"
                    aria-keyshortcuts="Meta+F Control+F"
                    prop:value=move || filter.get()
                    on:input=move |ev| filter.set(event_target_value(&ev))
                    on:keydown=move |ev| match ev.key().as_str() {
                        "ArrowDown" => {
                            ev.prevent_default();
                            focus_active_row();
                        }
                        "Enter" => {
                            ev.prevent_default();
                            toggle_first_match();
                        }
                        "Escape" => {
                            ev.prevent_default();
                            if filter.get_untracked().is_empty() {
                                mode.set("chat");
                                crate::dom::focus_message_input();
                            } else {
                                filter.set(String::new());
                            }
                        }
                        _ => {}
                    } />
                <div class="ctx-view-toggle" role="group" aria-label="File list view">
                    <button type="button" class="ctx-view-btn" class:on=move || !selected_only.get()
                        aria-pressed=move || if selected_only.get() { "false" } else { "true" }
                        on:click=move |_| selected_only.set(false)>"All"</button>
                    <button type="button" class="ctx-view-btn" class:on=move || selected_only.get()
                        aria-pressed=move || if selected_only.get() { "true" } else { "false" }
                        on:click=move |_| selected_only.set(true)>"Selected"</button>
                </div>
            </div>

            <div id="context-file-list" class="ctx-tree" role="listbox" aria-label="Workspace files. Use arrow keys to move, type to jump." aria-multiselectable="true"
                aria-activedescendant=move || format!("context-row-{}", active_row.get())>
                {move || {
                    let rows = visible_files(&files.get(), selected_only.get());
                    if rows.is_empty() {
                        let empty = if selected_only.get() { "No selected files match." } else { "No files match." };
                        view! { <div class="ctx-empty" role="status">{empty}</div> }.into_any()
                    } else {
                        let len = rows.len();
                        rows.into_iter().enumerate().map(|(index, f)| {
                            let path = f.path.clone();
                            let key_path = path.clone();
                            let display_path = f.display_path.clone();
                            let sel = f.selected;
                            let label = if sel {
                                format!("{display_path}. Selected. Press Enter to remove from context.")
                            } else {
                                format!("{display_path}. Not selected. Press Enter to add to context.")
                            };
                            let pos = (index + 1).to_string();
                            let size = len.to_string();
                            view! {
                                <button type="button" id=format!("context-row-{index}") class="ctx-row" class:on=sel class:active=move || active_row.get() == index
                                    role="option"
                                    tabindex=move || if active_row.get() == index { "0" } else { "-1" }
                                    aria-selected=if sel { "true" } else { "false" }
                                    aria-label=label
                                    aria-posinset=pos
                                    aria-setsize=size
                                    aria-keyshortcuts="ArrowUp ArrowDown Home End Escape Meta+A Control+A Delete Backspace"
                                    on:focus=move |_| active_row.set(index)
                                    on:keydown=move |ev| {
                                        let key = ev.key();
                                        if (ev.meta_key() || ev.ctrl_key()) && key.eq_ignore_ascii_case("a") {
                                            ev.prevent_default();
                                            select_shown.run(());
                                            return;
                                        }
                                        if (key == "Delete" || key == "Backspace") && sel {
                                            ev.prevent_default();
                                            toggle(key_path.clone(), true);
                                            return;
                                        }
                                        if ev.key() == "Escape" {
                                            ev.prevent_default();
                                            crate::dom::focus_context_filter();
                                            return;
                                        }
                                        if let Some(next) = context_row_target(index, len, &ev.key()) {
                                            ev.prevent_default();
                                            active_row.set(next);
                                            crate::dom::focus_context_row(next);
                                            return;
                                        }
                                        if !(ev.meta_key() || ev.ctrl_key() || ev.alt_key())
                                            && typeahead_jump(ev.key())
                                        {
                                            ev.prevent_default();
                                        }
                                    }
                                    on:click=move |_| toggle(path.clone(), sel)>
                                    <span class="ctx-check">{if sel { "☑" } else { "☐" }}</span>
                                    <span class="ctx-row-path">{display_path}</span>
                                </button>
                            }
                        }).collect_view().into_any()
                    }
                }}
                {move || truncated.get().then(|| view! {
                    <div class="ctx-trunc" role="status">"…more files — narrow the filter"</div>
                })}
            </div>

            <SelectionManifest
                selection=selection
                copy_manifest=copy_manifest
                remove_file=remove_selected_file
            />

            {move || view! { <BudgetPanel budget=budget.get()/> }}

            <div class="ctx-preview-head">
                <span>"Assembled context"</span>
                <div class="ctx-preview-actions">
                    <button type="button" class="ctx-copy" aria-label="Copy RepoPrompt-style context handoff" aria-keyshortcuts="Meta+Shift+C Control+Shift+C" on:click=copy_handoff>"Copy handoff"</button>
                    {move || native_file_dialogs.get().then(|| view! {
                        <button type="button" class="ctx-copy"
                            disabled=move || saving_handoff.get()
                            aria-label="Save RepoPrompt-style context handoff with native save panel"
                            aria-controls="context-operation-status"
                            on:click=save_handoff>
                            {move || if saving_handoff.get() { "Saving…" } else { "Save handoff…" }}
                        </button>
                    })}
                    <button type="button" class="ctx-copy" aria-label="Copy assembled context" on:click=copy>"Copy"</button>
                </div>
            </div>
            <pre class="ctx-preview" aria-label="Assembled context preview">{move || context_text.get()}</pre>
        </aside>
    }
}

fn context_handoff_file_name(workspace: &str, recipe: &str) -> String {
    let workspace = truncate_file_name_segment(&file_name_segment(workspace, "workspace"), 48);
    let recipe = truncate_file_name_segment(&file_name_segment(recipe, "standard"), 32);
    format!("nerve-{workspace}-{recipe}-handoff.md")
}

fn file_name_segment(value: &str, fallback: &'static str) -> String {
    let segment: String = value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' => ch,
            _ => '-',
        })
        .collect();
    let trimmed = segment.trim_matches('-');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn truncate_file_name_segment(segment: &str, max_chars: usize) -> String {
    segment.chars().take(max_chars).collect()
}

fn recipe_label(id: &str) -> &'static str {
    RECIPES
        .iter()
        .find_map(|(recipe_id, label)| (*recipe_id == id).then_some(*label))
        .unwrap_or("Custom")
}
