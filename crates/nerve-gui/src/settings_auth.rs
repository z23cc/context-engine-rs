//! Broker OAuth controls for the Settings modal.
//!
//! The Web GUI only stages browser OAuth and submits a pasted callback/code back
//! to the daemon. Token exchange and storage stay inside the host auth manager.

use crate::rpc::start_job_await;
use leptos::prelude::*;
use serde_json::{Value, json};

const AUTH_PROVIDER_OPTS: &[(&str, &str)] = &[
    ("claude", "Claude"),
    ("chatgpt", "ChatGPT/OpenAI"),
    ("xai", "xAI/Grok"),
];
const NO_TOKEN: &str = "No daemon token — open the daemon URL or append #token=…";

#[component]
pub(crate) fn BrokerOAuthControls(token: StoredValue<Option<String>>) -> impl IntoView {
    let provider = RwSignal::new("chatgpt".to_string());
    let auth_status = RwSignal::new("No auth status requested.".to_string());
    let auth_busy = RwSignal::new(false);
    let login_id = RwSignal::new(String::new());
    let authorize_url = RwSignal::new(String::new());
    let callback_input = RwSignal::new(String::new());
    let login_status = RwSignal::new("No browser login started.".to_string());
    let login_busy = RwSignal::new(false);
    let open_url_busy = RwSignal::new(false);
    let lease_status = RwSignal::new("No broker lease requested.".to_string());
    let lease_busy = RwSignal::new(false);
    let check_auth = move || request_auth_status(token, provider, auth_status, auth_busy);
    let start_login = move || {
        request_auth_start(
            token,
            provider,
            login_id,
            authorize_url,
            login_status,
            login_busy,
        );
    };
    let complete_login = move || {
        request_auth_complete(token, login_id, callback_input, login_status, login_busy);
    };
    let open_authorization = move || {
        request_open_authorization_url(token, authorize_url, login_status, open_url_busy);
    };
    let lease = move |force_refresh| {
        request_auth_lease(token, provider, lease_status, lease_busy, force_refresh);
    };
    view! {
        <div class="lease-box">
            <select id="auth-provider" name="auth-provider" class="set-select" prop:value=move || provider.get()
                aria-label="Auth provider"
                aria-controls="auth-status-output login-status-output lease-status-output"
                disabled=move || auth_busy.get() || login_busy.get() || lease_busy.get()
                on:change=move |ev| provider.set(event_target_value(&ev))>
                {AUTH_PROVIDER_OPTS.iter().map(|(id, label)| view! { <option value=*id>{*label}</option> }).collect_view()}
            </select>
            <div class="lease-actions">
                <button class="btn" type="button" disabled=move || auth_busy.get()
                    aria-label="Check selected provider auth status"
                    aria-controls="auth-status-output"
                    on:click=move |_| check_auth()>"Check status"</button>
                <button class="btn" type="button" disabled=move || login_busy.get()
                    aria-label="Start browser login for selected provider"
                    aria-controls="login-status-output"
                    on:click=move |_| start_login()>"Start browser login"</button>
                <button class="btn" type="button"
                    disabled=move || login_busy.get() || login_id.get().is_empty() || callback_input.get().trim().is_empty()
                    aria-label="Complete browser login with pasted callback"
                    aria-controls="login-status-output"
                    on:click=move |_| complete_login()>"Complete login"</button>
                <button class="btn" type="button" disabled=move || lease_busy.get()
                    aria-label="Check broker lease for selected provider"
                    aria-controls="lease-status-output"
                    on:click=move |_| lease(false)>"Check lease"</button>
                <button class="btn" type="button" disabled=move || lease_busy.get() title="Forces broker refresh/rotation"
                    aria-label="Force refresh broker lease for selected provider"
                    aria-controls="lease-status-output"
                    on:click=move |_| lease(true)>"Force refresh lease"</button>
            </div>
            <button class="btn auth-open" type="button"
                hidden=move || authorize_url.get().is_empty()
                disabled=move || open_url_busy.get()
                aria-label="Open authorization URL with the host system browser"
                aria-controls="login-status-output"
                on:click=move |_| open_authorization()>"Open authorization URL"</button>
            <input id="auth-callback" name="auth-callback" class="set-input auth-callback" type="text" placeholder="Paste callback URL or code"
                spellcheck="false"
                aria-label="Callback URL or authorization code"
                aria-describedby="login-status-output"
                prop:value=move || callback_input.get()
                disabled=move || login_busy.get()
                on:input=move |ev| callback_input.set(event_target_value(&ev)) />
            <pre id="auth-status-output" class="lease-status auth-status" role="status" aria-live="polite" aria-busy=move || auth_busy.get().to_string()>{move || auth_status.get()}</pre>
            <pre id="login-status-output" class="lease-status login-status" role="status" aria-live="polite" aria-busy=move || login_busy.get().to_string()>{move || login_status.get()}</pre>
            <pre id="lease-status-output" class="lease-status" role="status" aria-live="polite" aria-busy=move || lease_busy.get().to_string()>{move || lease_status.get()}</pre>
        </div>
    }
}

fn request_open_authorization_url(
    token: StoredValue<Option<String>>,
    authorize_url: RwSignal<String>,
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
    let url = authorize_url.get_untracked();
    if url.trim().is_empty() {
        status.set("No authorization URL to open.".into());
        return;
    }
    busy.set(true);
    status.set("Opening authorization URL in the system browser…".into());
    leptos::task::spawn_local(async move {
        status.set(match crate::data::open_host_url(&tok, &url).await {
            Ok(()) => "Opened authorization URL in the system browser.".into(),
            Err(err) => format!("Open authorization URL failed: {err}"),
        });
        busy.set(false);
    });
}

fn request_auth_status(
    token: StoredValue<Option<String>>,
    provider: RwSignal<String>,
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
    let provider_name = provider.get_untracked();
    busy.set(true);
    status.set(format!("Checking auth capabilities for {provider_name}…"));
    leptos::task::spawn_local(async move {
        let cmd = json!({ "kind": "auth.status", "provider": provider_name });
        status.set(match start_job_await(&tok, cmd).await {
            Ok(result) => format_auth_status(&result),
            Err(_) => "Auth status failed. Check daemon logs for details.".into(),
        });
        busy.set(false);
    });
}

fn request_auth_start(
    token: StoredValue<Option<String>>,
    provider: RwSignal<String>,
    login_id: RwSignal<String>,
    authorize_url: RwSignal<String>,
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
    let provider_name = provider.get_untracked();
    login_id.set(String::new());
    authorize_url.set(String::new());
    busy.set(true);
    status.set(format!("Starting browser login for {provider_name}…"));
    leptos::task::spawn_local(async move {
        let cmd = json!({ "kind": "auth.start", "provider": provider_name, "flow": "browser" });
        match start_job_await(&tok, cmd).await {
            Ok(result) => {
                login_id.set(str_field(&result, "login_id", ""));
                authorize_url.set(str_field(&result, "authorize_url", ""));
                status.set(format_auth_start(&result));
            }
            Err(_) => status.set("Auth start failed. Check daemon logs for details.".into()),
        }
        busy.set(false);
    });
}

fn request_auth_complete(
    token: StoredValue<Option<String>>,
    login_id: RwSignal<String>,
    callback_input: RwSignal<String>,
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
    let login = login_id.get_untracked();
    let pasted = callback_input.get_untracked();
    let pasted = pasted.trim().to_string();
    if login.is_empty() || pasted.is_empty() {
        status.set("Start login and paste the callback URL or code first.".into());
        return;
    }
    busy.set(true);
    status.set("Completing browser login via daemon broker…".into());
    leptos::task::spawn_local(async move {
        status.set(
            match start_job_await(&tok, auth_complete_command(&login, &pasted)).await {
                Ok(result) => format_auth_complete(&result),
                Err(_) => "Auth complete failed. Check daemon logs for details.".into(),
            },
        );
        busy.set(false);
    });
}

fn request_auth_lease(
    token: StoredValue<Option<String>>,
    provider: RwSignal<String>,
    status: RwSignal<String>,
    busy: RwSignal<bool>,
    force_refresh: bool,
) {
    let Some(tok) = token.get_value() else {
        status.set(NO_TOKEN.into());
        return;
    };
    if busy.get_untracked() {
        return;
    }
    let provider_name = provider.get_untracked();
    busy.set(true);
    status.set(format!(
        "Requesting broker lease metadata for {provider_name}{}…",
        if force_refresh {
            " (forces broker refresh)"
        } else {
            ""
        }
    ));
    leptos::task::spawn_local(async move {
        let cmd = json!({
            "kind": "auth.lease",
            "provider": provider_name,
            "force_refresh": force_refresh,
            "include_token": false,
        });
        status.set(match start_job_await(&tok, cmd).await {
            Ok(result) => format_auth_lease(&result),
            Err(err) => format_auth_lease_error(&err.to_string()),
        });
        busy.set(false);
    });
}

fn auth_complete_command(login_id: &str, pasted: &str) -> Value {
    if looks_like_callback_url(pasted) {
        json!({ "kind": "auth.complete", "login_id": login_id, "callback_url": pasted })
    } else {
        json!({ "kind": "auth.complete", "login_id": login_id, "code": pasted })
    }
}

fn looks_like_callback_url(pasted: &str) -> bool {
    pasted.starts_with("http://")
        || pasted.starts_with("https://")
        || pasted.starts_with('?')
        || pasted.contains("code=")
        || pasted.contains("error=")
}

fn format_auth_start(result: &Value) -> String {
    let provider = safe_status_text(&str_field(result, "provider", "unknown"));
    let login_id = safe_status_text(&str_field(result, "login_id", "unknown"));
    let redirect_uri = safe_status_text(&str_field(result, "redirect_uri", "unknown"));
    format!(
        "Browser login started: {provider}\nLogin id: {login_id}\nRedirect URI: {redirect_uri}\nOpen authorization URL, then paste the redirected callback URL or code below."
    )
}

fn format_auth_complete(result: &Value) -> String {
    let credential = result.get("credential").unwrap_or(result);
    let provider = safe_status_text(&str_field(credential, "provider", "unknown"));
    let status = safe_status_text(&str_field(credential, "status", "authenticated"));
    let mode = safe_status_text(&str_field(credential, "mode", "oauth"));
    format!(
        "Login complete: {provider} · {status} · mode={mode}\nAccess token: stored by daemon; not returned to Web GUI\nRefresh token: stored by daemon if provided; never returned to this client"
    )
}

fn format_auth_status(result: &Value) -> String {
    let provider = safe_status_text(&str_field(result, "provider", "unknown"));
    let status = safe_status_text(&str_field(result, "status", "unknown"));
    let mode = safe_status_text(&str_field(result, "mode", "none"));
    let caps = result.get("capabilities").unwrap_or(&Value::Null);
    let browser = caps
        .pointer("/auth_start/browser/supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let device = caps
        .pointer("/auth_start/device_code/supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let device_reason = safe_status_text(&str_pointer(
        caps,
        "/auth_start/device_code/reason",
        "provider device-code endpoints are not wired yet",
    ));
    let lease_available = caps
        .pointer("/auth_lease/metadata/available")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let lease_reason = safe_status_text(&str_pointer(
        caps,
        "/auth_lease/metadata/reason",
        "not_logged_in",
    ));
    let bearer_exposed = caps
        .pointer("/auth_lease/bearer_token/supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    format!(
        "Auth status: {provider} · {status} · mode={mode}\nBrowser login: {}\nDevice-code login: {}{}\nLease metadata: {}{}\nRuntime bearer: {}\nStored refresh: never returned",
        if browser {
            "supported"
        } else {
            "not supported"
        },
        if device { "supported" } else { "not supported" },
        if device {
            String::new()
        } else {
            format!(" ({device_reason})")
        },
        if lease_available {
            "available"
        } else {
            "unavailable"
        },
        if lease_available {
            String::new()
        } else {
            format!(" ({lease_reason})")
        },
        if bearer_exposed {
            "not requested by Web GUI"
        } else {
            "not exposed through runtime"
        }
    )
}

fn safe_status_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if [
        "bearer ",
        "access_token",
        "refresh_token",
        "sk-",
        "ghp_",
        "xoxb-",
        "{\"",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        "[redacted daemon text]".into()
    } else {
        text.to_string()
    }
}

fn format_auth_lease(result: &Value) -> String {
    let provider = str_field(result, "provider", "unknown");
    let status = str_field(result, "status", "unknown");
    let account = str_field(result, "account_id", "unknown");
    let expires = result
        .get("expires_at_unix")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |value| value.to_string());
    let access_line = if result
        .get("access_token_included")
        .and_then(Value::as_bool)
        .unwrap_or(result.get("access_token").is_some())
    {
        "Access token: not displayed"
    } else {
        "Access token: not returned to Web GUI"
    };
    let refresh_line = if result
        .get("refresh_token_held_by_broker")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "Refresh token: held by trusted broker; never returned to this client"
    } else {
        "Refresh token: never returned to this client"
    };
    format!(
        "Broker lease: {provider} · {status}\nBase URL: {}\nAccount: {account}\nExpires at Unix: {expires}\n{access_line}\n{refresh_line}",
        str_field(result, "base_url", "unknown")
    )
}

fn format_auth_lease_error(_error: &str) -> String {
    "Auth lease failed. Check daemon logs for details.".into()
}

fn str_pointer(value: &Value, pointer: &str, fallback: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn str_field(value: &Value, key: &str, fallback: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_complete_command_distinguishes_code_from_callback_url() {
        assert_eq!(auth_complete_command("l", "abc")["code"], "abc");
        assert!(
            auth_complete_command("l", "abc")
                .get("callback_url")
                .is_none()
        );
        let url = "http://localhost:1455/auth/callback?code=c&state=s";
        assert_eq!(auth_complete_command("l", url)["callback_url"], url);
    }

    #[test]
    fn format_auth_start_does_not_print_authorize_url_or_secret_shapes() {
        let out = format_auth_start(&json!({
            "provider": "openai",
            "login_id": "login-123",
            "redirect_uri": "http://localhost:1455/auth/callback",
            "authorize_url": "https://auth.example/?access_token=secret&state=s",
        }));
        assert!(out.contains("Browser login started: openai"));
        assert!(out.contains("Login id: login-123"));
        for forbidden in [
            "https://auth.example",
            "access_token",
            "secret",
            "Bearer ",
            "{\"",
        ] {
            assert!(!out.contains(forbidden), "leaked auth start text: {out}");
        }
    }

    #[test]
    fn format_auth_complete_reports_credential_without_secret_shapes() {
        let out = format_auth_complete(&json!({
            "login_id": "login-123",
            "credential": {
                "provider": "openai",
                "status": "authenticated",
                "mode": "oauth",
                "access_token": "access-secret",
                "refresh_token": "refresh-secret"
            }
        }));
        assert!(out.contains("Login complete: openai · authenticated · mode=oauth"));
        for forbidden in [
            "access-secret",
            "refresh-secret",
            "access_token",
            "refresh_token",
            "Bearer ",
            "{\"",
        ] {
            assert!(!out.contains(forbidden), "leaked auth complete text: {out}");
        }
    }

    #[test]
    fn format_auth_lease_redacts_secret_shapes() {
        let out = format_auth_lease(&json!({
            "provider": "openai",
            "status": "leased",
            "base_url": "https://api.openai.com",
            "account_id": "acct_123",
            "expires_at_unix": 12345,
            "access_token": "access-secret",
            "access_token_included": true,
            "refresh_token_held_by_broker": true,
        }));
        assert!(out.contains("Broker lease: openai"));
        assert!(out.contains("Access token: not displayed"));
        assert!(
            out.contains("Refresh token: held by trusted broker; never returned to this client")
        );
        for forbidden in [
            "access-secret",
            "Bearer ",
            "access_token",
            "refresh_token",
            "{\"",
        ] {
            assert!(!out.contains(forbidden), "leaked secret-shaped text: {out}");
        }
    }

    #[test]
    fn format_auth_lease_metadata_only_never_claims_token_received() {
        let out = format_auth_lease(&json!({
            "provider": "openai",
            "status": "leased",
            "access_token_included": false,
            "refresh_token_held_by_broker": true,
        }));
        assert!(out.contains("Access token: not returned to Web GUI"));
        assert!(!out.contains("received"));
    }

    #[test]
    fn format_auth_lease_error_is_generic() {
        let out =
            format_auth_lease_error("Bearer access-secret access_token refresh_token {\"x\":1}");
        assert_eq!(out, "Auth lease failed. Check daemon logs for details.");
    }

    #[test]
    fn format_auth_status_reports_capabilities_without_secret_shapes() {
        let out = format_auth_status(&json!({
            "provider": "openai",
            "status": "not_logged_in",
            "capabilities": {
                "auth_start": {
                    "browser": { "supported": true },
                    "device_code": {
                        "supported": false,
                        "reason": "provider device-authorization endpoints are not wired yet"
                    }
                },
                "auth_lease": {
                    "metadata": {
                        "supported": true,
                        "available": false,
                        "reason": "not_logged_in"
                    },
                    "bearer_token": {
                        "supported": false,
                        "reason": "not_exposed_over_runtime_protocol"
                    },
                    "stored_refresh_token": {
                        "supported": false,
                        "reason": "never_returned"
                    }
                }
            }
        }));
        assert!(out.contains("Auth status: openai · not_logged_in"));
        assert!(out.contains("Browser login: supported"));
        assert!(out.contains("Device-code login: not supported"));
        assert!(out.contains("Lease metadata: unavailable (not_logged_in)"));
        assert!(out.contains("Runtime bearer: not exposed through runtime"));
        for forbidden in ["access_token", "refresh_token", "Bearer ", "{\""] {
            assert!(!out.contains(forbidden), "leaked secret-shaped text: {out}");
        }
    }

    #[test]
    fn format_auth_status_redacts_secret_shaped_reasons() {
        let out = format_auth_status(&json!({
            "capabilities": {
                "auth_start": {
                    "browser": { "supported": true },
                    "device_code": { "supported": false, "reason": "Bearer abc access_token refresh_token {\"x\":1}" }
                },
                "auth_lease": {
                    "metadata": { "available": false, "reason": "sk-secret ghp_secret xoxb-secret" },
                    "bearer_token": { "supported": true }
                }
            }
        }));
        assert!(out.contains("Device-code login: not supported ([redacted daemon text])"));
        assert!(out.contains("Lease metadata: unavailable ([redacted daemon text])"));
        assert!(out.contains("Runtime bearer: not requested by Web GUI"));
        for forbidden in [
            "access_token",
            "refresh_token",
            "Bearer ",
            "sk-",
            "ghp_",
            "xoxb-",
            "{\"",
            "exposed",
        ] {
            assert!(!out.contains(forbidden), "leaked or misleading text: {out}");
        }
    }
}
