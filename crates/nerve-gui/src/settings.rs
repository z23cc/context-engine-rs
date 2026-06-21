//! Settings: the persisted defaults (theme + default agent/autonomy/model) and
//! the settings modal that edits them. Everything applies-on-select and persists
//! immediately to one `localStorage` key (`nerve.settings`) — there is no Save
//! button. Theme is applied by toggling `data-theme` on `<html>`; a tiny inline
//! script in `index.html` does the same pre-paint so there is no flash.
//!
//! Split out of `app.rs` to stay under the file-size gate.

use leptos::prelude::*;

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
    pub(crate) agent: String,
    pub(crate) autonomy: String,
    pub(crate) model: String,
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
    Settings {
        theme: get("theme", "system"),
        agent: get("agent", "claude"),
        autonomy: get("autonomy", "full"),
        model: get("model", ""),
    }
}

/// Persist the current settings (called from an Effect on any change).
pub(crate) fn save(s: &Settings) {
    let v = serde_json::json!({
        "theme": s.theme, "agent": s.agent, "autonomy": s.autonomy, "model": s.model,
    });
    if let Some(store) = local_storage() {
        let _ = store.set_item(KEY, &v.to_string());
    }
}

/// Drive `data-theme` on `<html>`: explicit `light`/`dark` set an override;
/// `system` (or anything else) clears it so the CSS media query governs (and
/// tracks the OS live, with no listener needed).
pub(crate) fn apply_theme(choice: &str) {
    let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.document_element())
    else {
        return;
    };
    match choice {
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

/// A segmented (radio-style) picker bound to a string signal.
fn seg(opts: &'static [(&'static str, &'static str)], sig: RwSignal<String>) -> impl IntoView {
    opts.iter()
        .map(|&(val, label)| {
            view! {
                <button class="seg-btn" type="button"
                    class:on=move || sig.get() == val
                    on:click=move |_| sig.set(val.to_string())>{label}</button>
            }
        })
        .collect_view()
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

/// The settings modal. Reuses the approval modal's scrim/card. Click-scrim and
/// the Done button close it; Escape is handled by the composer-level key path.
#[component]
pub(crate) fn SettingsModal(
    open: RwSignal<bool>,
    theme: RwSignal<String>,
    agent: RwSignal<String>,
    autonomy: RwSignal<String>,
    model: RwSignal<String>,
) -> impl IntoView {
    view! {
        <div class="modal-scrim" on:click=move |_| open.set(false)>
            <div class="modal settings-modal" role="dialog" aria-modal="true"
                on:click=move |ev| ev.stop_propagation()>
                <div class="modal-head"><span class="modal-title">"Settings"</span></div>
                <div class="set-body">
                    <Section title="Appearance" desc="Theme used across the app.">
                        <div class="seg">{seg(THEME_OPTS, theme)}</div>
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Default agent" desc="Which local CLI new threads use.">
                        <div class="seg">{seg(AGENT_OPTS, agent)}</div>
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Default autonomy" desc="Approval posture for new threads.">
                        <div class="seg">{seg(AUTO_OPTS, autonomy)}</div>
                    </Section>
                    <hr class="set-div"/>
                    <Section title="Default model" desc="Empty uses the CLI's own configured model.">
                        <select class="set-select" prop:value=move || model.get()
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
                    <button class="btn allow" on:click=move |_| open.set(false)>"Done"</button>
                </div>
            </div>
        </div>
    }
}
