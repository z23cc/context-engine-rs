//! Topbar chrome: workspace identity, surface tabs, model picker, inspector.
//! Split out of `app.rs` to keep the root view under the file-size gate.

use leptos::{ev, leptos_dom::helpers::window_event_listener, prelude::*};
use wasm_bindgen::JsCast;

#[component]
pub(crate) fn Topbar(
    agent: RwSignal<String>,
    model: RwSignal<String>,
    mode: RwSignal<&'static str>,
    workspace: RwSignal<String>,
    branch: RwSignal<String>,
    inspector_open: RwSignal<bool>,
    open_command_palette: Callback<()>,
    toggle_inspector: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="topbar">
            <div class="topbar-left">
                <div class="topbar-product">
                    <span class="topbar-eyebrow">"Nerve Runtime"</span>
                    <span class="topbar-status" role="status" aria-live="polite" aria-label=move || workspace_label(&workspace.get(), &branch.get())>{move || {
                        let ws = workspace.get();
                        let br = branch.get();
                        if ws.is_empty() { "No workspace selected".to_string() } else { format!("{ws} · {br}") }
                    }}</span>
                </div>
                <SurfaceTabs mode=mode/>
            </div>
            <div class="picker">
                <button type="button"
                    class="icon-btn command-key"
                    title="Command palette"
                    aria-label="Command palette"
                    aria-keyshortcuts="Meta+K Control+K"
                    on:click=move |_| {
                        crate::dom::close_model_picker_silent();
                        open_command_palette.run(());
                    }
                >"⌘K"</button>
                <ModelPicker
                    agent=agent
                    model=model
                />
                <button type="button" class="icon-btn inspector-toggle" title="Inspector"
                    aria-label=move || if inspector_open.get() { "Hide Inspector panel" } else { "Show Inspector panel" }
                    aria-controls="inspector-panel"
                    aria-expanded=move || inspector_open.get().to_string()
                    aria-keyshortcuts="Meta+I Control+I" on:click=move |_| {
                        crate::dom::close_model_picker_silent();
                        toggle_inspector.run(());
                    }>
                    <span>"Inspector"</span><span>"⊞"</span>
                </button>
            </div>
        </div>
    }
}

#[component]
fn ModelPicker(agent: RwSignal<String>, model: RwSignal<String>) -> impl IntoView {
    let picker_open = RwSignal::new(false);
    let clickaway = window_event_listener(ev::click, move |ev| {
        if !picker_open.get_untracked() {
            return;
        }
        let inside_picker = ev
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|element| element.closest("#model-menu").ok().flatten())
            .is_some();
        if !inside_picker {
            picker_open.set(false);
            crate::dom::close_model_picker_silent();
        }
    });
    on_cleanup(move || clickaway.remove());

    view! {
        <details id="model-menu" class="model-menu"
            on:toggle=move |ev| {
                let open_now = ev
                    .target()
                    .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                    .is_some_and(|target| target.has_attribute("open"));
                picker_open.set(open_now);
                if open_now {
                    crate::dom::focus_model_agent_select();
                }
            }
            on:keydown=move |ev| match ev.key().as_str() {
                "Escape" if picker_open.get_untracked() => {
                    ev.prevent_default();
                    picker_open.set(false);
                    crate::dom::close_model_picker();
                }
                "Tab" if picker_open.get_untracked() && crate::dom::trap_tab_focus("model-menu", ev.shift_key()) => {
                    ev.prevent_default();
                }
                _ => {}
            }
        >
            <summary id="model-picker-summary" class="model-pill" title="Model picker"
                aria-haspopup="dialog"
                aria-controls="model-picker-popover"
                aria-expanded=move || if picker_open.get() { "true" } else { "false" }
                aria-describedby="model-picker-current model-picker-help"
                aria-keyshortcuts="Escape"
                aria-label=move || format!(
                    "Model picker: {}, {}",
                    picker_agent_label(&agent.get()),
                    picker_model_label(&agent.get(), &model.get())
                )>
                <span class="model-agent">{move || picker_agent_label(&agent.get())}</span>
                <span class="model-dot">"·"</span>
                <span class="model-choice">{move || picker_model_label(&agent.get(), &model.get())}</span>
                <span class="model-chevron">"⌄"</span>
            </summary>
            <div id="model-picker-popover" class="model-popover" role="dialog" aria-labelledby="model-picker-summary" aria-describedby="model-picker-current model-picker-help">
                <div id="model-picker-current" class="model-current" role="status" aria-live="polite">
                    {move || picker_current_label(&agent.get(), &model.get())}
                </div>
                <div id="model-picker-help" class="model-help">
                    "Changes apply immediately. Escape closes; Tab stays inside this picker."
                </div>
                <label>
                    <span>"Agent"</span>
                    <select id="model-agent-select" class="pick-in wide" title="Agent CLI" aria-label="Agent CLI" aria-describedby="model-picker-help"
                        prop:value=move || agent.get()
                        on:change=move |ev| agent.set(event_target_value(&ev))>
                        {crate::data::AGENTS.iter().map(|(id, label)| view! {
                            <option value=*id>{*label}</option>
                        }).collect_view()}
                    </select>
                </label>
                <label>
                    <span>"Model"</span>
                    <select id="model-select" class="pick-in wide" title="Model"
                        aria-label=move || crate::data::model_control_label(&agent.get(), &model.get())
                        aria-describedby="model-picker-current model-picker-help"
                        prop:value=move || model.get()
                        on:change=move |ev| model.set(event_target_value(&ev))>
                        {move || {
                            let ag = agent.get();
                            crate::data::AGENT_MODELS.iter()
                                .filter(move |(a, _, _)| *a == ag)
                                .map(|(_, id, label)| view! { <option value=*id>{*label}</option> })
                                .collect_view()
                        }}
                    </select>
                </label>
            </div>
        </details>
    }
}

#[component]
fn SurfaceTabs(mode: RwSignal<&'static str>) -> impl IntoView {
    view! {
        <div
            class="topbar-tabs"
            role="tablist"
            aria-label="Main surfaces"
            on:keydown=move |ev| {
                if let Some(next) = surface_for_key(mode.get_untracked(), &ev.key()) {
                    ev.prevent_default();
                    crate::dom::close_model_picker_silent();
                    mode.set(next);
                    crate::dom::focus_surface(next);
                }
            }
        >
            <button type="button"
                id="surface-tab-chat"
                class="topbar-tab"
                class:on=move || mode.get() == "chat"
                role="tab"
                aria-controls="surface-chat"
                aria-selected=move || if mode.get() == "chat" { "true" } else { "false" }
                aria-keyshortcuts="Meta+1 Control+1"
                tabindex=move || if mode.get() == "chat" { "0" } else { "-1" }
                on:click=move |_| {
                    crate::dom::close_model_picker_silent();
                    mode.set("chat");
                    crate::dom::focus_surface("chat");
                }
            >"Chat"</button>
            <button type="button"
                id="surface-tab-context"
                class="topbar-tab"
                class:on=move || mode.get() == "context"
                role="tab"
                aria-controls="surface-context"
                aria-selected=move || if mode.get() == "context" { "true" } else { "false" }
                aria-keyshortcuts="Meta+2 Control+2"
                tabindex=move || if mode.get() == "context" { "0" } else { "-1" }
                on:click=move |_| {
                    crate::dom::close_model_picker_silent();
                    mode.set("context");
                    crate::dom::focus_surface("context");
                }
            >"Context"</button>
        </div>
    }
}

fn workspace_label(workspace: &str, branch: &str) -> String {
    if workspace.is_empty() {
        "No workspace selected".into()
    } else {
        format!("Workspace {workspace}, branch {branch}")
    }
}

fn picker_agent_label(agent: &str) -> String {
    crate::data::agent_label(agent).into()
}

fn picker_model_label(agent: &str, model: &str) -> String {
    crate::data::model_label(agent, model).into()
}

fn picker_current_label(agent: &str, model: &str) -> String {
    format!(
        "Current model: {} with {}",
        crate::data::model_label(agent, model),
        crate::data::agent_label(agent)
    )
}

fn surface_for_key(current: &'static str, key: &str) -> Option<&'static str> {
    match (current, key) {
        (_, "Home") | (_, "ArrowLeft") if current == "context" => Some("chat"),
        (_, "End") | (_, "ArrowRight") if current == "chat" => Some("context"),
        _ => None,
    }
}
