//! Topbar chrome: command affordances, model picker, context toggle, inspector.
//! Split out of `app.rs` to keep the root view under the file-size gate.

use leptos::prelude::*;

#[component]
pub(crate) fn Topbar(
    agent: RwSignal<String>,
    model: RwSignal<String>,
    mode: RwSignal<&'static str>,
    toggle_inspector: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="topbar">
            <div class="picker">
                <button
                    class="icon-btn"
                    title="Terminal — coming soon"
                    aria-label="Terminal — coming soon"
                    aria-disabled="true"
                    on:click=move |ev| ev.prevent_default()
                >"⌘J"</button>
                <details class="model-menu">
                    <summary class="model-pill" title="Model picker">
                        <span class="model-agent">{move || crate::data::agent_label(&agent.get()).to_string()}</span>
                        <span class="model-dot">"·"</span>
                        <span class="model-choice">{move || crate::data::model_label(&agent.get(), &model.get()).to_string()}</span>
                        <span class="model-chevron">"⌄"</span>
                    </summary>
                    <div class="model-popover" role="group" aria-label="Model picker">
                        <label>
                            <span>"Agent"</span>
                            <select
                                class="pick-in wide"
                                title="Agent CLI"
                                prop:value=move || agent.get()
                                on:change=move |ev| agent.set(event_target_value(&ev))
                            >
                                {crate::data::AGENTS.iter().map(|(id, label)| view! {
                                    <option value=*id>{*label}</option>
                                }).collect_view()}
                            </select>
                        </label>
                        <label>
                            <span>"Model"</span>
                            <select
                                class="pick-in wide"
                                title="Model"
                                prop:value=move || model.get()
                                on:change=move |ev| model.set(event_target_value(&ev))
                            >
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
                <button class="mode-toggle" title="Context builder"
                    on:click=move |_| mode.update(|m| *m = if *m == "context" { "chat" } else { "context" })>
                    {move || if mode.get() == "context" { "← Chat" } else { "Context" }}
                </button>
                <button class="icon-btn" title="Task pane" on:click=move |_| toggle_inspector.run(())>"⊞"</button>
                <button
                    class="icon-btn"
                    title="Pop out — coming soon"
                    aria-label="Pop out — coming soon"
                    aria-disabled="true"
                    on:click=move |ev| ev.prevent_default()
                >"↗"</button>
            </div>
        </div>
    }
}
