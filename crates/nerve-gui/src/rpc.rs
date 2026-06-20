//! Protocol-v4 client plumbing for the WASM frontend: read the daemon bearer
//! token the served page injected, and POST JSON-RPC requests to `/rpc`.
//!
//! The token is injected by the daemon the same way the legacy `gui.html` is
//! served — as a `window.__NERVE_DAEMON_TOKEN__` global (see the daemon's
//! `render_app` token injection). On a remote bind the daemon does not embed
//! it, so the global is the unreplaced placeholder; in that case the operator
//! supplies it via the URL fragment and we fall back to that.

use std::sync::atomic::{AtomicU64, Ordering};

use gloo_net::http::Request;
use nerve_proto::RuntimeEvent;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_sys::{EventSource, MessageEvent};

/// The placeholder the daemon replaces with the real token on a loopback bind.
/// If we still see this string, no token was injected (remote bind).
const TOKEN_PLACEHOLDER: &str = "__NERVE_DAEMON_TOKEN__";

/// Read the daemon bearer token: prefer the injected `window.__NERVE_DAEMON_TOKEN__`
/// global; on a remote bind (placeholder unreplaced) fall back to a `#token=`
/// URL fragment. Returns `None` when neither yields a usable token.
pub fn daemon_token() -> Option<String> {
    if let Some(tok) = injected_token() {
        return Some(tok);
    }
    fragment_token()
}

/// The token baked into the page by the daemon, if it is a real (replaced) value.
fn injected_token() -> Option<String> {
    let window = web_sys::window()?;
    let value = js_sys::Reflect::get(&window, &"__NERVE_DAEMON_TOKEN__".into()).ok()?;
    let tok = value.as_string()?;
    (!tok.is_empty() && tok != TOKEN_PLACEHOLDER).then_some(tok)
}

/// A `#token=<tok>` URL fragment, used on a remote bind where the page carries
/// no embedded token (the operator opens the `#token=` URL the daemon printed).
fn fragment_token() -> Option<String> {
    let hash = web_sys::window()?.location().hash().ok()?;
    let hash = hash.strip_prefix('#').unwrap_or(&hash);
    hash.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "token" && !v.is_empty()).then(|| v.to_string())
    })
}

/// One JSON-RPC call against `/rpc`, deserializing `result` into `T`.
///
/// `T` is a [`nerve_proto`] response type, so the WASM app shares the engine's
/// exact wire shape. Errors collapse to a human string for the placeholder UI;
/// richer error surfacing arrives with the real chat surface (G2).
pub async fn rpc_call<T: DeserializeOwned>(
    token: &str,
    method: &str,
    params: Value,
) -> Result<T, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let response = Request::post("/rpc")
        .header("Authorization", &format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .map_err(|err| format!("encode request: {err}"))?
        .send()
        .await
        .map_err(|err| format!("POST /rpc failed: {err}"))?;
    if !response.ok() {
        return Err(format!("/rpc returned HTTP {}", response.status()));
    }
    let envelope: Value = response
        .json()
        .await
        .map_err(|err| format!("decode /rpc response: {err}"))?;
    if let Some(err) = envelope.get("error") {
        return Err(format!("JSON-RPC error: {err}"));
    }
    let result = envelope
        .get("result")
        .ok_or_else(|| "response missing `result`".to_string())?
        .clone();
    serde_json::from_value(result).map_err(|err| format!("deserialize {method} result: {err}"))
}

/// Monotonic counter so each `runtime/jobs/start` carries a unique `job_id`.
static JOB_SEQ: AtomicU64 = AtomicU64::new(0);

/// A unique client-side job id (single-threaded wasm: a timestamp + counter).
fn next_job_id() -> String {
    let n = JOB_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("gui-{}-{n}", js_sys::Date::now() as u64)
}

/// Start a job carrying the `RuntimeCommand` `command` (e.g. `session.start` /
/// `session.message`); returns the JSON-RPC `result` value. The session lifecycle
/// rides the existing job machinery — no new transport.
pub async fn start_job(token: &str, command: Value) -> Result<Value, String> {
    rpc_call::<Value>(
        token,
        "runtime/jobs/start",
        json!({ "job_id": next_job_id(), "command": command }),
    )
    .await
}

/// Open the `/events` SSE stream and invoke `on_event` for each decoded
/// [`RuntimeEvent`]. The token rides the URL query (an `EventSource` cannot set
/// headers — same as the legacy gui.html). The `EventSource` + its closure are
/// leaked deliberately: the stream lives for the app's lifetime and the browser
/// auto-reconnects (replaying via `Last-Event-ID`).
pub fn open_events(token: &str, on_event: impl Fn(RuntimeEvent) + 'static) -> Result<(), String> {
    let url = format!("/events?token={token}");
    let source = EventSource::new(&url).map_err(|err| format!("open /events: {err:?}"))?;
    let handler = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
        let Some(data) = message.data().as_string() else {
            return;
        };
        // Each frame is a JSON-RPC notification `{method:"runtime/event", params:<RuntimeEvent>}`.
        let Ok(note) = serde_json::from_str::<Value>(&data) else {
            return;
        };
        let params = note.get("params").cloned().unwrap_or(Value::Null);
        if let Ok(event) = serde_json::from_value::<RuntimeEvent>(params) {
            on_event(event);
        }
    });
    source.set_onmessage(Some(handler.as_ref().unchecked_ref()));
    handler.forget();
    std::mem::forget(source);
    Ok(())
}
