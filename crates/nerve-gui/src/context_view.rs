//! The Context view: pick files from a clickable tree, assemble a token-budgeted
//! context with a named recipe, then copy/export it — the RepoPrompt-style "build
//! context for an LLM" surface. All deterministic work lives in the engine
//! (`list_files` / `manage_selection` / `workspace_context` + recipes); this is a
//! thin client over `tool.call`.

// View-heavy component; see app.rs for why too_many_lines is allowed at module
// scope (the `#[component]` macro drops a fn-level allow).
#![allow(clippy::too_many_lines)]

use crate::data::{FileRow, fetch_context, fetch_diff, list_files, selection_op};
use leptos::prelude::*;

const RECIPES: &[(&str, &str)] = &[
    ("standard", "Standard"),
    ("plan", "Plan"),
    ("review", "Review"),
    ("diff", "Diff Follow-Up"),
    ("manual", "Manual"),
];

const FILE_LIMIT: usize = 1500;

/// The token breakdown returned under `workspace_context` `structuredContent.tokens`.
#[derive(Clone, Default, serde::Deserialize)]
struct Budget {
    total_tokens: usize,
    file_map_tokens: usize,
    contents_tokens: usize,
    git_diff_tokens: usize,
    meta_prompts_tokens: usize,
    instructions_tokens: usize,
}

#[component]
pub fn ContextView(
    token: StoredValue<Option<String>>,
    /// The active workspace name, threaded into every tool call so the Context
    /// surface follows the selected project.
    workspace: RwSignal<String>,
) -> impl IntoView {
    let recipe = RwSignal::new("standard".to_string());
    let filter = RwSignal::new(String::new());
    let files = RwSignal::new(Vec::<FileRow>::new());
    let truncated = RwSignal::new(false);
    let context_text = RwSignal::new(String::new());
    let budget = RwSignal::new(Budget::default());

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

    // Toggle a file's selection, then refresh the list state + the budget.
    let toggle = move |path: String, selected: bool| {
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        let op = if selected { "remove" } else { "add" };
        leptos::task::spawn_local(async move {
            let _ = selection_op(&tok, op, vec![path], &ws).await;
            load_files();
            refresh();
        });
    };

    let copy = move |_| {
        let text = context_text.get_untracked();
        if let Some(clip) = web_sys::window().map(|w| w.navigator().clipboard()) {
            let _ = clip.write_text(&text);
        }
    };

    view! {
        <aside class="context-view">
            <div class="ctx-head">
                <span class="ctx-title">"Context"</span>
                <select class="ctx-recipe" prop:value=move || recipe.get()
                    on:change=move |ev| recipe.set(event_target_value(&ev))>
                    {RECIPES.iter().map(|(id, label)| view! { <option value=*id>{*label}</option> }).collect_view()}
                </select>
            </div>

            <input class="ctx-add-in" placeholder="filter files…"
                prop:value=move || filter.get()
                on:input=move |ev| filter.set(event_target_value(&ev)) />

            <div class="ctx-tree">
                {move || {
                    let rows = files.get();
                    if rows.is_empty() {
                        view! { <div class="ctx-empty">"No files match."</div> }.into_any()
                    } else {
                        rows.into_iter().map(|f| {
                            let path = f.path.clone();
                            let sel = f.selected;
                            view! {
                                <button class="ctx-row" class:on=sel
                                    on:click=move |_| toggle(path.clone(), sel)>
                                    <span class="ctx-check">{if sel { "☑" } else { "☐" }}</span>
                                    <span class="ctx-row-path">{f.display_path}</span>
                                </button>
                            }
                        }).collect_view().into_any()
                    }
                }}
                {move || truncated.get().then(|| view! {
                    <div class="ctx-trunc">"…more files — narrow the filter"</div>
                })}
            </div>

            <div class="ctx-budget">
                <div class="ctx-total">{move || budget.get().total_tokens}" tokens"</div>
                <div class="ctx-bars">
                    {move || {
                        let b = budget.get();
                        view! {
                            <Bar label="files" n=b.contents_tokens total=b.total_tokens/>
                            <Bar label="map" n=b.file_map_tokens total=b.total_tokens/>
                            <Bar label="diff" n=b.git_diff_tokens total=b.total_tokens/>
                            <Bar label="meta" n=b.meta_prompts_tokens total=b.total_tokens/>
                            <Bar label="instr" n=b.instructions_tokens total=b.total_tokens/>
                        }
                    }}
                </div>
            </div>

            <div class="ctx-preview-head">
                <span>"Assembled context"</span>
                <button class="ctx-copy" on:click=copy>"Copy"</button>
            </div>
            <pre class="ctx-preview">{move || context_text.get()}</pre>
        </aside>
    }
}

#[component]
fn Bar(label: &'static str, n: usize, total: usize) -> impl IntoView {
    let pct = (n * 100).checked_div(total).unwrap_or(0);
    view! {
        <div class="ctx-bar-row">
            <span class="ctx-bar-label">{label}</span>
            <span class="ctx-bar-track"><span class="ctx-bar-fill" style=format!("width:{pct}%")></span></span>
            <span class="ctx-bar-n">{n}</span>
        </div>
    }
}
