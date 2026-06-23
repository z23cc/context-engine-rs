//! Settings: the persisted defaults (theme + default agent/autonomy/model) and
//! the settings modal that edits them. Everything applies-on-select and persists
//! immediately to one `localStorage` key (`nerve.settings`) — there is no Save
//! button. Theme is applied by toggling `data-theme` on `<html>`; a tiny inline
//! script in `index.html` does the same pre-paint so there is no flash.
//!
//! Split out of `app.rs` to stay under the file-size gate.

// View-heavy settings dialog; keep the grouped form in one component so the
// keyboard trap, signals, and labeled controls stay auditable together.
#![allow(clippy::too_many_lines)]

use crate::host_capabilities::HostCapabilitiesPanel;
use crate::settings_auth::BrokerOAuthControls;
use leptos::{ev, leptos_dom::helpers::window_event_listener, prelude::*};
use nerve_proto::HostCapabilities;
use wasm_bindgen::JsCast;

const KEY: &str = "nerve.settings";

const THEME_OPTS: &[(&str, &str)] = &[("system", "System"), ("light", "Light"), ("dark", "Dark")];
const AGENT_OPTS: &[(&str, &str)] = &[("claude", "Claude Code"), ("codex", "Codex")];
const AUTO_OPTS: &[(&str, &str)] = &[
    ("full", "Full access"),
    ("edit", "Auto-edit"),
    ("read_only", "Read-only"),
];

/// The persisted defaults. Strings (not enums) so the segmented pickers bind
/// uniformly and the values flow straight to `delegate.start`.
pub(crate) struct Settings {
    pub(crate) theme: String,
    pub(crate) accent: String,
    pub(crate) bg: String,
    pub(crate) fg: String,
    pub(crate) font_ui: String,
    pub(crate) font_code: String,
    pub(crate) sidebar_vibrancy: bool,
    pub(crate) chat_backend: String,
    pub(crate) agent: String,
    pub(crate) autonomy: String,
    pub(crate) model: String,
    pub(crate) runtime_provider: String,
    pub(crate) runtime_model: String,
}

pub(crate) fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// Read the persisted settings, falling back to sane defaults for any missing key.
pub(crate) fn load() -> Settings {
    let raw = local_storage()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    let get = |k: &str, d: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or(d)
            .to_string()
    };
    let get_bool = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    Settings {
        theme: get("theme", "system"),
        accent: get("accent", ""),
        bg: get("bg", ""),
        fg: get("fg", ""),
        font_ui: get("font_ui", ""),
        font_code: get("font_code", ""),
        sidebar_vibrancy: get_bool("sidebar_vibrancy"),
        chat_backend: get("chat_backend", "delegate"),
        agent: get("agent", "claude"),
        autonomy: get("autonomy", "full"),
        model: get("model", ""),
        runtime_provider: get("runtime_provider", "anthropic"),
        runtime_model: get("runtime_model", "claude-sonnet-4"),
    }
}

/// Persist the current settings (called from an Effect on any change).
pub(crate) fn save(s: &Settings) {
    let v = serde_json::json!({
        "theme": s.theme,
        "accent": s.accent,
        "bg": s.bg,
        "fg": s.fg,
        "font_ui": s.font_ui,
        "font_code": s.font_code,
        "sidebar_vibrancy": s.sidebar_vibrancy,
        "chat_backend": s.chat_backend,
        "agent": s.agent,
        "autonomy": s.autonomy,
        "model": s.model,
        "runtime_provider": s.runtime_provider,
        "runtime_model": s.runtime_model,
    });
    if let Some(store) = local_storage() {
        let _ = store.set_item(KEY, &v.to_string());
    }
}

/// Drive `data-theme` and Codex-style theme knobs on `<html>`. Base theme
/// chooses system/light/dark; non-empty overrides map directly to CSS variables.
/// Blank accent follows the host system accent when the daemon can report it,
/// otherwise it falls back to the stylesheet tokens.
pub(crate) fn apply_theme(settings: &Settings, host_caps: Option<&HostCapabilities>) {
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    match settings.theme.as_str() {
        "light" => {
            let _ = el.set_attribute("data-theme", "light");
        }
        "dark" => {
            let _ = el.set_attribute("data-theme", "dark");
        }
        _ => {
            let _ = el.remove_attribute("data-theme");
        }
    }
    let Some(html) = el.dyn_ref::<web_sys::HtmlElement>() else {
        return;
    };
    let style = html.style();
    apply_accent_vars(&style, settings, host_caps);
    set_var(&style, "--bg", &settings.bg);
    set_var(&style, "--fg", &settings.fg);
    set_var(&style, "--font-ui", &settings.font_ui);
    set_var(&style, "--font-code", &settings.font_code);
    if settings.sidebar_vibrancy {
        let _ = el.set_attribute("data-vibrancy", "sidebar");
    } else {
        let _ = el.remove_attribute("data-vibrancy");
    }
}

fn apply_accent_vars(
    style: &web_sys::CssStyleDeclaration,
    settings: &Settings,
    host_caps: Option<&HostCapabilities>,
) {
    if settings.accent.trim().is_empty() {
        set_var_optional(
            style,
            "--accent",
            host_caps.and_then(|caps| caps.system_accent_color.as_deref()),
        );
        set_var_optional(
            style,
            "--accent-ink",
            host_caps.and_then(|caps| caps.system_accent_ink_color.as_deref()),
        );
    } else {
        set_var(style, "--accent", &settings.accent);
        let _ = style.remove_property("--accent-ink");
    }
}

fn set_var(style: &web_sys::CssStyleDeclaration, name: &str, value: &str) {
    set_var_optional(style, name, Some(value));
}

fn set_var_optional(style: &web_sys::CssStyleDeclaration, name: &str, value: Option<&str>) {
    let trimmed = value.unwrap_or_default().trim();
    if trimmed.is_empty() {
        let _ = style.remove_property(name);
    } else {
        let _ = style.set_property(name, trimmed);
    }
}

/// localStorage key for persisted conversation history.
const CHATS_KEY: &str = "nerve.chats";

/// Persist the conversation list (titles + turns + timestamps) so history survives
/// a restart. serde skips the runtime fields (session/turn_job/streaming); a
/// best-effort write (quota errors are ignored).
pub(crate) fn save_chats(chats: &[crate::app::Chat]) {
    if let (Some(store), Ok(json)) = (local_storage(), serde_json::to_string(chats)) {
        let _ = store.set_item(CHATS_KEY, &json);
    }
}

/// Load persisted conversation history (empty when none / unparseable).
pub(crate) fn load_chats() -> Vec<crate::app::Chat> {
    local_storage()
        .and_then(|s| s.get_item(CHATS_KEY).ok().flatten())
        .and_then(|raw| serde_json::from_str::<Vec<crate::app::Chat>>(&raw).ok())
        .unwrap_or_default()
}

/// A segmented radio picker bound to a string signal.
fn seg(
    group: &'static str,
    label: &'static str,
    opts: &'static [(&'static str, &'static str)],
    sig: RwSignal<String>,
) -> impl IntoView {
    let len = opts.len();
    view! {
        <div class="seg" role="radiogroup" aria-label=label>
            {opts.iter().enumerate().map(|(index, &(val, label))| {
                view! {
                    <button id=format!("settings-{group}-{index}") class="seg-btn" type="button"
                        role="radio"
                        class:on=move || sig.get() == val
                        aria-checked=move || if sig.get() == val { "true" } else { "false" }
                        tabindex=move || if sig.get() == val { "0" } else { "-1" }
                        on:click=move |_| sig.set(val.to_string())
                        on:keydown=move |ev| {
                            if let Some(next) = segment_key_target(index, len, &ev.key()) {
                                ev.prevent_default();
                                if let Some((next_val, _)) = opts.get(next) {
                                    sig.set((*next_val).to_string());
                                    crate::dom::focus_settings_segment(group, next);
                                }
                            }
                        }>{label}</button>
                }
            }).collect_view()}
        </div>
    }
}

fn segment_key_target(current: usize, len: usize, key: &str) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match key {
        "ArrowRight" | "ArrowDown" => Some((current + 1) % len),
        "ArrowLeft" | "ArrowUp" => Some((current + len - 1) % len),
        "Home" => Some(0),
        "End" => Some(len - 1),
        _ => None,
    }
}

fn token_input(
    sig: RwSignal<String>,
    id: &'static str,
    label: &'static str,
    placeholder: &'static str,
) -> impl IntoView {
    view! {
        <input
            id=id
            name=id
            class="set-input"
            type="text"
            spellcheck="false"
            placeholder=placeholder
            aria-label=label
            prop:value=move || sig.get()
            on:input=move |ev| sig.set(event_target_value(&ev))
        />
    }
}

fn bool_toggle(sig: RwSignal<bool>, label: &'static str) -> impl IntoView {
    view! {
        <button class="set-toggle" type="button" class:on=move || sig.get()
            aria-pressed=move || sig.get().to_string()
            aria-label=label
            on:click=move |_| sig.update(|enabled| *enabled = !*enabled)>
            <span class="set-toggle-dot"></span>{label}
        </button>
    }
}

#[component]
fn Section(title: &'static str, desc: &'static str, children: Children) -> impl IntoView {
    view! {
        <div class="set-section">
            <div class="set-text">
                <h2 class="set-title">{title}</h2>
                <p class="set-desc">{desc}</p>
            </div>
            <div class="set-control">{children()}</div>
        </div>
    }
}

/// The settings modal. Reuses the approval modal's scrim/card. Click-scrim,
/// Escape, and the Done button close it.
#[component]
pub(crate) fn SettingsModal(
    open: RwSignal<bool>,
    token: StoredValue<Option<String>>,
    theme: RwSignal<String>,
    accent: RwSignal<String>,
    bg: RwSignal<String>,
    fg: RwSignal<String>,
    font_ui: RwSignal<String>,
    font_code: RwSignal<String>,
    sidebar_vibrancy: RwSignal<bool>,
    agent: RwSignal<String>,
    autonomy: RwSignal<String>,
    model: RwSignal<String>,
    mode: RwSignal<&'static str>,
) -> impl IntoView {
    let close_settings = Callback::new(move |_| {
        open.set(false);
        crate::dom::focus_surface(mode.get_untracked());
    });
    let close_key = close_settings;
    let keydown = window_event_listener(ev::keydown, move |ev| {
        if !open.get_untracked() {
            return;
        }
        match ev.key().as_str() {
            "Escape" => {
                ev.prevent_default();
                close_key.run(());
            }
            "Tab" if crate::dom::trap_tab_focus("settings-dialog", ev.shift_key()) => {
                ev.prevent_default();
            }
            _ => {}
        }
    });
    on_cleanup(move || keydown.remove());
    Effect::new(move |_| {
        if open.get() {
            crate::dom::focus_settings_segment("theme", 0);
        }
    });

    view! {
        <div class="modal-scrim" hidden=move || !open.get() on:click=move |_| close_settings.run(())>
            <div id="settings-dialog" class="modal settings-modal" role="dialog" aria-modal="true" aria-labelledby="settings-title" aria-describedby="settings-desc settings-shortcuts" tabindex="-1"
                on:click=move |ev| ev.stop_propagation()>
                <div class="modal-head"><span id="settings-title" class="modal-title">"Settings"</span></div>
                <p id="settings-desc" class="set-desc">"Preferences apply immediately and persist for new GUI sessions."</p>
                <div id="settings-shortcuts" class="modal-shortcuts" aria-label="Settings keyboard shortcuts">
                    <span><kbd>"Esc"</kbd>" Close"</span>
                    <span><kbd>"Tab"</kbd>" Move focus"</span>
                </div>
                <div class="set-body">
                    <Section title="Appearance" desc="Base theme used across the app.">
                        {seg("theme", "Appearance theme", THEME_OPTS, theme)}
                    </Section>
                    <Section title="Accent" desc="Optional CSS color for --accent; blank follows the host system accent when available.">
                        {token_input(accent, "settings-accent", "Accent CSS color", "system")}
                    </Section>
                    <Section title="Background" desc="Optional CSS color for --bg.">
                        {token_input(bg, "settings-background", "Background CSS color", "#fbfbfa")}
                    </Section>
                    <Section title="Foreground" desc="Optional CSS color for --fg.">
                        {token_input(fg, "settings-foreground", "Foreground CSS color", "#131312")}
                    </Section>
                    <Section title="UI font" desc="Optional CSS font-family for --font-ui.">
                        {token_input(font_ui, "settings-font-ui", "UI font family", "-apple-system, Segoe UI, sans-serif")}
                    </Section>
                    <Section title="Code font" desc="Optional CSS font-family for --font-code.">
                        {token_input(font_code, "settings-font-code", "Code font family", "ui-monospace, SF Mono, monospace")}
                    </Section>
                    <Section title="Sidebar material" desc="Optional translucent sidebar for macOS/WebKit shells.">
                        {bool_toggle(sidebar_vibrancy, "Vibrant sidebar")}
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Host capabilities" desc="Native affordances currently exposed by this runtime host.">
                        <HostCapabilitiesPanel token=token/>
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Broker OAuth" desc="Start browser OAuth, complete pasted callbacks, and inspect metadata-only broker lease status.">
                        <BrokerOAuthControls token=token/>
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Default agent" desc="Which external CLI new threads launch (Claude Code / Codex).">
                        {seg("agent", "Default agent", AGENT_OPTS, agent)}
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Default autonomy" desc="Approval posture for new threads.">
                        {seg("autonomy", "Default autonomy", AUTO_OPTS, autonomy)}
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Default model" desc="Empty uses the CLI's own configured model.">
                        <select id="settings-default-model" name="settings-default-model" class="set-select" prop:value=move || model.get()
                            aria-label=move || format!("Default {}", crate::data::model_control_label(&agent.get(), &model.get()))
                            on:change=move |ev| model.set(event_target_value(&ev))>
                            {move || {
                                let ag = agent.get();
                                crate::data::AGENT_MODELS.iter()
                                    .filter(move |(a, _, _)| *a == ag)
                                    .map(|(_, id, label)| view! { <option value=*id>{*label}</option> })
                                    .collect_view()
                            }}
                        </select>
                    </Section>
                </div>
                <div class="modal-actions">
                    <button type="button" class="btn allow" aria-label="Close settings" aria-keyshortcuts="Escape" on:click=move |_| close_settings.run(())>"Done"</button>
                </div>
            </div>
        </div>
    }
}
