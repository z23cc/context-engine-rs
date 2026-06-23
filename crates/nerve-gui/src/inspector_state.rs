//! Inspector tab state and daemon-backed data loading.

use leptos::prelude::*;

pub(crate) struct InspectorState {
    pub(crate) tab: RwSignal<&'static str>,
    pub(crate) data: RwSignal<String>,
    pub(crate) load_tab: Callback<&'static str>,
    pub(crate) toggle: Callback<()>,
}

pub(crate) fn inspector_state(
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    open: RwSignal<bool>,
    mode: RwSignal<&'static str>,
) -> InspectorState {
    let tab = RwSignal::new("changes");
    let data = RwSignal::new(String::new());
    let generation = StoredValue::new(0u32);

    let load_tab = Callback::new(move |next: &'static str| {
        tab.set(next);
        focus_tab_pane(next);
        if next == "plan" || next == "sessions" {
            return;
        }
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        let current_generation = generation.get_value() + 1;
        generation.set_value(current_generation);
        data.set("loading…".into());
        leptos::task::spawn_local(async move {
            let text = match next {
                "files" => crate::data::fetch_file_tree(&tok, &ws).await,
                "changes" | "review" => crate::data::fetch_diff(&tok, &ws).await,
                _ => None,
            };
            if generation.get_value() == current_generation
                && tab.get_untracked() == next
                && workspace.get_untracked() == ws
            {
                data.set(text.unwrap_or_else(|| "—".into()));
            }
        });
    });

    let refresh_tab = load_tab;
    Effect::new(move |_| {
        let _ = workspace.get();
        if open.get() {
            let current_tab = tab.get_untracked();
            if current_tab != "plan" {
                refresh_tab.run(current_tab);
            }
        }
    });

    let toggle = Callback::new(move |_| {
        let was_open = open.get_untracked();
        open.set(!was_open);
        if was_open {
            crate::dom::focus_surface(mode.get_untracked());
        } else {
            focus_tab_pane(tab.get_untracked());
        }
    });

    InspectorState {
        tab,
        data,
        load_tab,
        toggle,
    }
}

fn focus_tab_pane(tab: &'static str) {
    let id = match tab {
        "plan" => "tool-panel",
        "sessions" => "sessions-panel",
        "files" => "files-panel",
        "changes" => "changes-panel",
        "review" => "review-panel",
        _ => return,
    };
    crate::dom::focus_element_by_id_next_frame(id);
}
