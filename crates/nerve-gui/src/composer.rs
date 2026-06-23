//! Composer control shared by the empty hero and docked chat bar.

// View-heavy component; the `#[component]` macro expands a declarative control
// tree, so splitting purely to satisfy the line counter would make the signal
// wiring harder to audit.
#![allow(clippy::too_many_lines)]

use leptos::prelude::*;

#[component]
pub(crate) fn Composer(
    agent: RwSignal<String>,
    autonomy: RwSignal<String>,
    branch: RwSignal<String>,
    input: RwSignal<String>,
    mode: RwSignal<&'static str>,
    model: RwSignal<String>,
    palette_open: RwSignal<bool>,
    workspace: RwSignal<String>,
    busy: Signal<bool>,
    send: Callback<()>,
    stop: Callback<()>,
    reveal: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="composer-stack">
            <div class="composer-box">
                <div class="composer-bar">
                    <div class="composer-modes" aria-label="Execution target">
                        <span class="composer-mode on" role="status" title="Run against the local workspace">"Local workspace"</span>
                    </div>
                    <div class="composer-affordances">
                        <button type="button"
                            class="tool-btn"
                            title="Add context"
                            aria-label="Add context"
                            on:click=move |_| {
                                mode.set("context");
                                crate::dom::focus_context_filter();
                            }
                        >"+"</button>
                        <button type="button"
                            class="tool-btn"
                            title="Command palette"
                            aria-label="Command palette"
                            on:click=move |_| palette_open.set(true)
                        >"⌘K"</button>
                    </div>
                </div>
                <div class="composer-input-row">
                    <textarea
                        id="message"
                        name="message"
                        class="input"
                        rows="1"
                        aria-label="Message composer. Press Enter to send, Shift Enter for a new line, or slash when empty for commands."
                        aria-keyshortcuts="Enter Shift+Enter /"
                        prop:value=move || input.get()
                        on:input=move |ev| input.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            let key = ev.key();
                            if key == "/" && input.with_untracked(|text| text.trim().is_empty()) {
                                ev.prevent_default();
                                palette_open.set(true);
                            } else if key == "Enter" && !ev.shift_key() {
                                ev.prevent_default();
                                send.run(());
                            }
                        }
                        placeholder="Describe a task…  /  for commands"
                    ></textarea>
                    <div class="composer-tools">
                        <select
                            id="composer-agent"
                            name="composer-agent"
                            class="effort"
                            title="Agent CLI"
                            aria-label="Agent CLI"
                            prop:value=move || agent.get()
                            on:change=move |ev| agent.set(event_target_value(&ev))
                        >
                            {crate::data::AGENTS.iter().map(|(id, label)| view! {
                                <option value=*id>{*label}</option>
                            }).collect_view()}
                        </select>
                        <select
                            id="composer-autonomy"
                            name="composer-autonomy"
                            class="access-pill"
                            title="Autonomy"
                            aria-label="Autonomy"
                            prop:value=move || autonomy.get()
                            on:change=move |ev| autonomy.set(event_target_value(&ev))
                        >
                            <option value="full" selected=move || autonomy.get() == "full">"Full access"</option>
                            <option value="edit" selected=move || autonomy.get() == "edit">"Auto-edit"</option>
                            <option value="read_only" selected=move || autonomy.get() == "read_only">"Read-only"</option>
                        </select>
                        <select
                            id="composer-model"
                            name="composer-model"
                            class="effort"
                            title="Model"
                            aria-label=move || crate::data::model_control_label(&agent.get(), &model.get())
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
                        {move || if busy.get() {
                            view! { <button class="send stop" type="button" title="Stop" aria-label="Stop response" on:click=move |_| stop.run(())>"■"</button> }.into_any()
                        } else {
                            view! { <button class="send" type="button" title="Send" aria-label="Send message" on:click=move |_| send.run(())>"↑"</button> }.into_any()
                        }}
                    </div>
                </div>
            </div>
            <div class="context-pills">
                <button type="button" class="ctx-pill ctx-pill-act" title="Reveal in Finder"
                    aria-label=move || format!("Reveal workspace {} in Finder", workspace.get())
                    on:click=move |_| reveal.run(())>"📁 "{move || workspace.get()}</button>
                <span class="ctx-pill" aria-label=move || format!("Agent: {}", crate::data::agent_label(&agent.get()))>{move || format!("Agent: {}", crate::data::agent_label(&agent.get()))}</span>
                <span class="ctx-pill" aria-label=move || format!("Git branch: {}", branch.get())>"⎇ "{move || branch.get()}</span>
            </div>
        </div>
    }
}
