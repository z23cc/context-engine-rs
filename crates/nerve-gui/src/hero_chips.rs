//! Empty-thread hero workflow shortcuts.

use leptos::prelude::*;

#[component]
pub(crate) fn HeroChips(
    input: RwSignal<String>,
    mode: RwSignal<&'static str>,
    open_review: Callback<()>,
    open_tools: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="hero-chips" role="group" aria-label="Quick start">
            <button type="button" class="hero-chip" on:click=move |_| {
                input.set("Inspect the relevant code first, then make a step-by-step plan for the next change.".into());
                crate::dom::focus_message_input();
            }>
                <span class="chip-icon" aria-hidden="true">
                    <svg class="chip-svg" viewBox="0 0 24 24">
                        <circle cx="4.5" cy="6" r="1.1"/><circle cx="4.5" cy="12" r="1.1"/><circle cx="4.5" cy="18" r="1.1"/>
                        <path d="M9 6h11M9 12h11M9 18h7"/>
                    </svg>
                </span>
                <span class="chip-title">"Plan a change"</span>
                <span class="chip-desc">"Inspect code, then a step-by-step plan"</span>
                <span class="hero-chip-key">"↵"</span>
            </button>
            <button type="button" class="hero-chip" on:click=move |_| {
                mode.set("context");
                crate::dom::focus_context_filter();
            }>
                <span class="chip-icon" aria-hidden="true">
                    <svg class="chip-svg" viewBox="0 0 24 24">
                        <path d="M12 3 3 7.5 12 12l9-4.5L12 3Z"/><path d="m3 12 9 4.5L21 12"/><path d="m3 16.5 9 4.5 9-4.5"/>
                    </svg>
                </span>
                <span class="chip-title">"Build context"</span>
                <span class="chip-desc">"Pick files, copy a handoff"</span>
                <span class="hero-chip-key">"⌘2"</span>
            </button>
            <button type="button" class="hero-chip" on:click=move |_| open_review.run(())>
                <span class="chip-icon" aria-hidden="true">
                    <svg class="chip-svg" viewBox="0 0 24 24">
                        <circle cx="7" cy="7" r="2.4"/><circle cx="17" cy="17" r="2.4"/>
                        <path d="M7 9.4v3.1a4 4 0 0 0 4 4h3M17 14.6v-3.1a4 4 0 0 0-4-4h-3"/>
                    </svg>
                </span>
                <span class="chip-title">"Review diff"</span>
                <span class="chip-desc">"Assemble a review packet"</span>
                <span class="hero-chip-key">"⌘3"</span>
            </button>
            <button type="button" class="hero-chip" on:click=move |_| open_tools.run(())>
                <span class="chip-icon" aria-hidden="true">
                    <svg class="chip-svg" viewBox="0 0 24 24">
                        <path d="M3 12h3.5l2.5 6 4-13 2.5 7H21"/>
                    </svg>
                </span>
                <span class="chip-title">"Tool activity"</span>
                <span class="chip-desc">"Watch tool calls live"</span>
                <span class="hero-chip-key">"⌘4"</span>
            </button>
        </div>
    }
}
