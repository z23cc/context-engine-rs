//! Command palette / quick actions overlay for Codex-style keyboard workflows.

// View-heavy component; see app.rs for why too_many_lines is allowed at module
// scope (the `#[component]` macro drops a fn-level allow).
#![allow(clippy::too_many_lines)]

use leptos::{ev, leptos_dom::helpers::window_event_listener, prelude::*};

use crate::command_catalog::{
    CommandTone, active_command, active_option_id, command_option_id, visible_commands,
};

#[component]
pub(crate) fn CommandPalette(
    open: RwSignal<bool>,
    mode: RwSignal<&'static str>,
    input: RwSignal<String>,
    token: StoredValue<Option<String>>,
    workspace: RwSignal<String>,
    chats: RwSignal<Vec<crate::app::Chat>>,
    active_thread: RwSignal<usize>,
    new_thread: Callback<()>,
    clear_thread: Callback<()>,
    toggle_inspector: Callback<()>,
    open_inspector_tab: Callback<&'static str>,
    settings_open: RwSignal<bool>,
    native_file_dialogs: Signal<bool>,
) -> impl IntoView {
    let query = RwSignal::new(String::new());
    let active = RwSignal::new(0usize);
    let command_note = RwSignal::new(String::new());

    Effect::new(move |_| {
        if open.get() {
            query.set(String::new());
            active.set(0);
            command_note.set(String::new());
            crate::dom::focus_command_search();
        }
    });

    Effect::new(move |_| {
        let q = query.get();
        let visible_len = visible_commands(&q, native_file_dialogs.get()).len();
        active.update(|idx| {
            if visible_len == 0 || *idx >= visible_len {
                *idx = 0;
            }
        });
    });

    let close_palette = Callback::new(move |_| {
        open.set(false);
        crate::dom::focus_surface(mode.get_untracked());
    });

    let run_plan = Callback::new(move |_| {
        mode.set("chat");
        input.set(
            "Inspect the relevant code first, then make a step-by-step plan for the next change."
                .into(),
        );
        open.set(false);
        crate::dom::focus_message_input();
    });
    let run_context = Callback::new(move |_| {
        mode.set("context");
        open.set(false);
        crate::dom::focus_context_filter();
    });
    let run_review_packet = Callback::new(move |_| {
        open_inspector_tab.run("review");
        open.set(false);
    });
    let run_tool_activity = Callback::new(move |_| {
        open_inspector_tab.run("plan");
        open.set(false);
    });
    let run_sessions = Callback::new(move |_| {
        open_inspector_tab.run("sessions");
        open.set(false);
    });
    let run_review = Callback::new(move |_| {
        mode.set("chat");
        input.set("Review the current git diff for correctness, risks, missing tests, and simplification opportunities.".into());
        open.set(false);
        crate::dom::focus_message_input();
    });
    let run_inspector = Callback::new(move |_| {
        toggle_inspector.run(());
        open.set(false);
    });
    let run_new_thread = Callback::new(move |_| {
        new_thread.run(());
        mode.set("chat");
        open.set(false);
        crate::dom::focus_message_input();
    });
    let run_clear_thread = Callback::new(move |_| {
        clear_thread.run(());
        open.set(false);
        crate::dom::focus_message_input();
    });
    let run_find = Callback::new(move |_| {
        open.set(false);
        if mode.get_untracked() == "context" {
            crate::dom::focus_context_filter();
        } else {
            crate::dom::focus_thread_search();
        }
    });
    let run_settings = Callback::new(move |_| {
        open.set(false);
        settings_open.set(true);
    });
    let run_copy_thread = Callback::new(move |_| {
        crate::artifact_export::copy_thread_transcript(chats, active_thread, command_note);
        crate::dom::focus_command_search();
    });
    let run_copy_context = Callback::new(move |_| {
        crate::artifact_export::copy_context_handoff(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_copy_manifest = Callback::new(move |_| {
        crate::artifact_export::copy_selection_manifest(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_copy_review = Callback::new(move |_| {
        crate::artifact_export::copy_review_packet(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_copy_tools = Callback::new(move |_| {
        crate::artifact_export::copy_tool_activity(chats, active_thread, command_note);
        crate::dom::focus_command_search();
    });
    let run_copy_tree = Callback::new(move |_| {
        crate::artifact_export::copy_file_tree(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_copy_bundle = Callback::new(move |_| {
        crate::artifact_export::copy_full_handoff_bundle(
            token,
            workspace,
            chats,
            active_thread,
            command_note,
        );
        crate::dom::focus_command_search();
    });
    let run_save_bundle = Callback::new(move |_| {
        crate::artifact_export::save_full_handoff_bundle(
            token,
            workspace,
            chats,
            active_thread,
            command_note,
        );
        crate::dom::focus_command_search();
    });
    let run_save_context = Callback::new(move |_| {
        crate::artifact_export::save_context_handoff(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_save_review = Callback::new(move |_| {
        crate::artifact_export::save_review_packet(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_save_thread = Callback::new(move |_| {
        crate::artifact_export::save_thread_transcript(token, chats, active_thread, command_note);
        crate::dom::focus_command_search();
    });
    let run_save_manifest = Callback::new(move |_| {
        crate::artifact_export::save_selection_manifest(token, workspace, command_note);
        crate::dom::focus_command_search();
    });
    let run_save_tools = Callback::new(move |_| {
        crate::artifact_export::save_tool_activity(token, chats, active_thread, command_note);
        crate::dom::focus_command_search();
    });
    let run_save_tree = Callback::new(move |_| {
        crate::artifact_export::save_file_tree(token, workspace, command_note);
        crate::dom::focus_command_search();
    });

    let run_command = Callback::new(move |id: &'static str| match id {
        "01" => run_plan.run(()),
        "02" => run_context.run(()),
        "03" => run_review_packet.run(()),
        "04" => run_tool_activity.run(()),
        "25" => run_sessions.run(()),
        "05" => run_review.run(()),
        "06" => run_inspector.run(()),
        "07" => run_new_thread.run(()),
        "08" => run_clear_thread.run(()),
        "09" => run_find.run(()),
        "10" => run_settings.run(()),
        "11" => run_copy_thread.run(()),
        "12" => run_copy_context.run(()),
        "13" => run_copy_manifest.run(()),
        "14" => run_copy_review.run(()),
        "15" => run_copy_tools.run(()),
        "16" => run_copy_tree.run(()),
        "17" => run_copy_bundle.run(()),
        "18" => run_save_bundle.run(()),
        "19" => run_save_context.run(()),
        "20" => run_save_review.run(()),
        "21" => run_save_thread.run(()),
        "22" => run_save_manifest.run(()),
        "23" => run_save_tools.run(()),
        "24" => run_save_tree.run(()),
        _ => {}
    });

    let key_run_command = run_command;
    let key_context = run_command;
    let key_review_packet = run_command;
    let key_tool_activity = run_command;
    let key_inspector = run_command;
    let key_new_thread = run_command;
    let key_clear_thread = run_command;
    let key_find = run_command;
    let key_settings = run_command;
    let key_copy_context = run_command;
    let key_copy_bundle = run_command;
    let keydown = window_event_listener(ev::keydown, move |ev| {
        if ev.default_prevented() {
            return;
        }
        let model_picker_open = crate::dom::element_has_attribute("model-menu", "open");
        if settings_open.get_untracked()
            || crate::dom::element_exists("approval-dialog")
            || model_picker_open
        {
            if model_picker_open && (ev.meta_key() || ev.ctrl_key()) {
                ev.prevent_default();
            }
            if open.get_untracked() {
                open.set(false);
            }
            return;
        }
        let key = ev.key();
        let chord = ev.meta_key() || ev.ctrl_key();
        if chord && ev.shift_key() && key.eq_ignore_ascii_case("b") {
            ev.prevent_default();
            open.set(true);
            query.set(String::new());
            active.set(0);
            key_copy_bundle.run("17");
            return;
        }
        if chord && ev.shift_key() && key.eq_ignore_ascii_case("c") {
            ev.prevent_default();
            key_copy_context.run("12");
            return;
        }
        if chord && key.eq_ignore_ascii_case("k") {
            ev.prevent_default();
            open.set(true);
            query.set(String::new());
            active.set(0);
            return;
        }
        if chord && key.eq_ignore_ascii_case("i") {
            ev.prevent_default();
            key_inspector.run("06");
            return;
        }
        if chord && key.eq_ignore_ascii_case("n") {
            ev.prevent_default();
            key_new_thread.run("07");
            return;
        }
        if chord && key.eq_ignore_ascii_case("f") {
            ev.prevent_default();
            key_find.run("09");
            return;
        }
        if chord && key == "," {
            ev.prevent_default();
            key_settings.run("10");
            return;
        }
        if chord && key == "1" {
            ev.prevent_default();
            mode.set("chat");
            open.set(false);
            crate::dom::focus_message_input();
            return;
        }
        if chord && key == "2" {
            ev.prevent_default();
            key_context.run("02");
            return;
        }
        if chord && key == "3" {
            ev.prevent_default();
            key_review_packet.run("03");
            return;
        }
        if chord && key == "4" {
            ev.prevent_default();
            key_tool_activity.run("04");
            return;
        }
        if key == "Escape" && open.get_untracked() {
            ev.prevent_default();
            close_palette.run(());
            return;
        }
        if !open.get_untracked() {
            return;
        }
        if key == "Tab" && crate::dom::trap_tab_focus("command-dialog", ev.shift_key()) {
            ev.prevent_default();
            return;
        }
        if chord && key == "Backspace" {
            ev.prevent_default();
            key_clear_thread.run("08");
            return;
        }

        let visible_len =
            visible_commands(&query.get_untracked(), native_file_dialogs.get_untracked()).len();
        if visible_len == 0 {
            return;
        }
        match key.as_str() {
            "ArrowDown" => {
                ev.prevent_default();
                active.update(|idx| *idx = (*idx + 1) % visible_len);
            }
            "ArrowUp" => {
                ev.prevent_default();
                active.update(|idx| *idx = if *idx == 0 { visible_len - 1 } else { *idx - 1 });
            }
            "Home" => {
                ev.prevent_default();
                active.set(0);
            }
            "End" => {
                ev.prevent_default();
                active.set(visible_len - 1);
            }
            "Enter" => {
                ev.prevent_default();
                if let Some(id) = active_command(
                    &query.get_untracked(),
                    active.get_untracked(),
                    native_file_dialogs.get_untracked(),
                ) {
                    key_run_command.run(id);
                }
            }
            _ => {}
        }
    });
    on_cleanup(move || keydown.remove());

    let rows_runner = run_command;
    view! {
        {move || (!open.get() && !command_note.get().is_empty()).then(|| view! {
            <div id="command-status" class="cmd-status-line" role="status" aria-live="polite" aria-atomic="true">
                <span class="cmd-status-label">"Command"</span>
                <span>{command_note.get()}</span>
            </div>
        })}
        {move || open.get().then(|| view! {
            <div class="cmd-scrim" role="presentation" on:click=move |_| close_palette.run(())>
                <section
                    id="command-dialog"
                    class="cmd-palette"
                    role="dialog"
                    aria-modal="true"
                    aria-labelledby="command-title"
                    aria-describedby="command-help"
                    on:click=move |ev| ev.stop_propagation()
                >
                    <div class="cmd-head">
                        <span id="command-title" class="cmd-kicker">"Command Palette"</span>
                        <button type="button" class="cmd-close" title="Close" aria-label="Close command palette" aria-keyshortcuts="Escape" on:click=move |_| close_palette.run(())>"Esc"</button>
                    </div>
                    <label class="cmd-search">
                        <span>"⌘K"</span>
                        <input
                            id="command-search"
                            type="search"
                            spellcheck="false"
                            role="combobox"
                            aria-label="Search command palette"
                            aria-keyshortcuts="Meta+K Control+K"
                            aria-controls="command-list"
                            aria-expanded="true"
                            aria-autocomplete="list"
                            aria-activedescendant=move || active_option_id(&query.get(), active.get(), native_file_dialogs.get())
                            aria-describedby="command-help"
                            placeholder="Search commands, workflows, context…"
                            prop:value=move || query.get()
                            on:input=move |ev| {
                                query.set(event_target_value(&ev));
                                active.set(0);
                            }
                        />
                    </label>
                    <div id="command-help" class="cmd-help">"↑↓ select · Enter run · Esc close · Tab stays here"</div>
                    {move || (!command_note.get().is_empty()).then(|| view! {
                        <div class="cmd-note" role="status">{command_note.get()}</div>
                    })}
                    <div
                        id="command-list"
                        class="cmd-list"
                        role="listbox"
                        aria-label="Commands"
                        aria-activedescendant=move || active_option_id(&query.get(), active.get(), native_file_dialogs.get())
                    >
                        {move || command_rows(&query.get(), active, rows_runner, native_file_dialogs.get())}
                    </div>
                </section>
            </div>
        })}
    }
}

fn command_rows(
    query: &str,
    active: RwSignal<usize>,
    run_command: Callback<&'static str>,
    native_file_dialogs: bool,
) -> AnyView {
    let rows = visible_commands(query, native_file_dialogs);
    if rows.is_empty() {
        return view! {
            <div class="cmd-empty" role="status">"No commands match “"{query.to_string()}"”."</div>
        }
        .into_any();
    }

    let len = rows.len();
    rows.into_iter()
        .enumerate()
        .map(|(idx, command)| {
            let pos = (idx + 1).to_string();
            let size = len.to_string();
            let label = format!(
                "{}: {}. Shortcut {}",
                command.title, command.desc, command.key
            );
            view! {
                <button type="button"
                    id=command_option_id(command.id)
                    class="cmd-row"
                    class:active=move || active.get() == idx
                    class:primary=move || command.tone == CommandTone::Primary
                    class:danger=move || command.tone == CommandTone::Danger
                    role="option"
                    aria-label=label
                    aria-selected=move || if active.get() == idx { "true" } else { "false" }
                    aria-posinset=pos
                    aria-setsize=size
                    tabindex="-1"
                    on:focus=move |_| active.set(idx)
                    on:mouseenter=move |_| active.set(idx)
                    on:click=move |_| run_command.run(command.id)
                >
                    <span class="cmd-icon">{command.id}</span>
                    <span class="cmd-copy"><b>{command.title}</b><em>{command.desc}</em></span>
                    <span class="cmd-key">{command.key}</span>
                </button>
            }
        })
        .collect_view()
        .into_any()
}
