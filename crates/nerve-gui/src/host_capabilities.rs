//! Host/native shell capability status for Settings.
//!
//! The Web GUI must not imply RepoPrompt/Codex-App-level native affordances it
//! cannot actually reach. This panel queries the runtime host seam and renders the
//! daemon's concrete capability matrix.

use crate::rpc::start_job_await;
use leptos::prelude::*;
use nerve_proto::HostCapabilities;
use serde_json::json;

const NO_TOKEN: &str = "No daemon token — open the daemon URL or append #token=…";

#[derive(Clone)]
enum HostCapsState {
    Idle,
    Loading,
    Ready(HostCapabilities),
    Error(String),
}

#[component]
pub(crate) fn HostCapabilitiesPanel(token: StoredValue<Option<String>>) -> impl IntoView {
    let state = RwSignal::new(HostCapsState::Idle);
    let busy = RwSignal::new(false);
    let notification_status = RwSignal::new(String::new());
    let notification_busy = RwSignal::new(false);
    Effect::new(move |_| {
        if matches!(state.get(), HostCapsState::Idle) {
            request_host_capabilities(token, state, busy);
        }
    });

    view! {
        <div class="host-caps-box">
            <div class="host-caps-actions">
                <button class="btn" type="button" disabled=move || busy.get()
                    aria-label="Refresh host capabilities"
                    aria-controls="host-capabilities-output"
                    on:click=move |_| request_host_capabilities(token, state, busy)>"Refresh"</button>
                <span class="host-caps-note">"Shows only capabilities this runtime host actually exposes."</span>
            </div>
            <div id="host-capabilities-output" role="status" aria-live="polite" aria-busy=move || busy.get().to_string()>
                {move || render_host_caps_state(
                    state.get(),
                    token,
                    notification_status,
                    notification_busy,
                )}
            </div>
        </div>
    }
}

fn request_host_capabilities(
    token: StoredValue<Option<String>>,
    state: RwSignal<HostCapsState>,
    busy: RwSignal<bool>,
) {
    let Some(tok) = token.get_value() else {
        state.set(HostCapsState::Error(NO_TOKEN.into()));
        return;
    };
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    state.set(HostCapsState::Loading);
    leptos::task::spawn_local(async move {
        state.set(match crate::data::fetch_host_capabilities(&tok).await {
            Ok(caps) => HostCapsState::Ready(caps),
            Err(err) => HostCapsState::Error(format!("Host capability query failed: {err}")),
        });
        busy.set(false);
    });
}

fn render_host_caps_state(
    state: HostCapsState,
    token: StoredValue<Option<String>>,
    notification_status: RwSignal<String>,
    notification_busy: RwSignal<bool>,
) -> AnyView {
    match state {
        HostCapsState::Idle | HostCapsState::Loading => view! {
            <div class="host-caps-empty">"Checking host capabilities…"</div>
        }
        .into_any(),
        HostCapsState::Error(message) => view! {
            <div class="host-caps-empty error">{message}</div>
        }
        .into_any(),
        HostCapsState::Ready(caps) => {
            render_host_caps_ready(caps, token, notification_status, notification_busy)
        }
    }
}

fn render_host_caps_ready(
    caps: HostCapabilities,
    token: StoredValue<Option<String>>,
    notification_status: RwSignal<String>,
    notification_busy: RwSignal<bool>,
) -> AnyView {
    let rows = capability_rows(&caps);
    let can_notify = caps.os_notifications;
    let system_scheme = caps.system_color_scheme.clone();
    let system_accent = caps.system_accent_color.clone();
    view! {
        <div class="host-caps-ready">
            <div class="host-caps-meta">
                <span>{"Host: "}{caps.host}</span>
                <span>{"Platform: "}{caps.platform}</span>
                {system_scheme.map(|scheme| view! { <span>{"System: "}{scheme}</span> })}
                {system_accent.map(|accent| view! { <span>{"Accent: "}{accent}</span> })}
            </div>
            {can_notify.then(|| view! {
                <div class="host-caps-test">
                    <button class="btn" type="button" disabled=move || notification_busy.get()
                        aria-label="Send test OS notification"
                        aria-controls="host-notification-test-output"
                        on:click=move |_| request_test_notification(token, notification_status, notification_busy)>
                        "Send test notification"
                    </button>
                    <span id="host-notification-test-output" class="host-caps-note" role="status" aria-live="polite" aria-busy=move || notification_busy.get().to_string()>
                        {move || notification_status.get()}
                    </span>
                </div>
            })}
            <div class="host-caps-grid" role="list" aria-label="Host-native capability matrix">
                {rows.into_iter().map(capability_row).collect_view()}
            </div>
        </div>
    }
    .into_any()
}

fn request_test_notification(
    token: StoredValue<Option<String>>,
    status: RwSignal<String>,
    busy: RwSignal<bool>,
) {
    let Some(tok) = token.get_value() else {
        status.set(NO_TOKEN.into());
        return;
    };
    if busy.get_untracked() {
        return;
    }
    busy.set(true);
    status.set("Sending OS notification…".into());
    leptos::task::spawn_local(async move {
        let command = json!({
            "kind": "host.notification.show",
            "title": "Nerve",
            "body": "Host OS notifications are available.",
        });
        status.set(match start_job_await(&tok, command).await {
            Ok(_) => "Sent test notification.".into(),
            Err(err) => format!("Notification failed: {err}"),
        });
        busy.set(false);
    });
}

fn capability_rows(caps: &HostCapabilities) -> Vec<(&'static str, bool, &'static str)> {
    vec![
        (
            "Workspace reveal",
            caps.workspace_reveal,
            "Open the served root in the OS file manager.",
        ),
        (
            "Native window chrome",
            caps.native_window_chrome,
            "Real title bar, window controls, and restoration.",
        ),
        (
            "Native settings window",
            caps.native_settings_window,
            "Separate OS settings surface instead of an in-window modal.",
        ),
        (
            "Native file dialogs",
            caps.native_file_dialogs,
            "Open/save panels owned by the host shell.",
        ),
        (
            "Global hotkey",
            caps.global_hotkey,
            "System-wide launcher shortcut handled below the WebView.",
        ),
        (
            "Native drag and drop",
            caps.native_drag_drop,
            "File URLs through the platform pasteboard/data object.",
        ),
        (
            "OS notifications",
            caps.os_notifications,
            "Notification center integration from the host.",
        ),
        (
            "External URL opener",
            caps.external_url_open,
            "Open browser/OAuth links through the OS default handler.",
        ),
        (
            "Host clipboard text",
            caps.clipboard_write_text,
            "Plain text writes through the daemon instead of browser clipboard permission.",
        ),
        (
            "Rich clipboard",
            caps.rich_clipboard,
            "Clipboard writes beyond plain text, such as HTML or RTF.",
        ),
        (
            "Native context menu",
            caps.native_context_menu,
            "Host-populated context menus instead of browser defaults.",
        ),
        (
            "System appearance",
            caps.system_color_scheme.is_some() || caps.system_accent_color.is_some(),
            "Host-reported color scheme and accent color for native-looking theming.",
        ),
    ]
}

fn capability_row((label, supported, desc): (&'static str, bool, &'static str)) -> impl IntoView {
    let state_class = if supported {
        "cap-state on"
    } else {
        "cap-state off"
    };
    let state_text = if supported {
        "Available"
    } else {
        "Unavailable"
    };
    view! {
        <div class="cap-row" role="listitem">
            <div>
                <div class="cap-title">{label}</div>
                <div class="cap-desc">{desc}</div>
            </div>
            <span class=state_class>{state_text}</span>
        </div>
    }
}
