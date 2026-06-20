//! Protocol-v4 client plumbing for the WASM frontend: read the daemon bearer
//! token the served page injected, and POST JSON-RPC requests to `/rpc`.
//!
//! The token is injected by the daemon the same way the legacy `gui.html` is
//! served — as a `window.__NERVE_DAEMON_TOKEN__` global (see the daemon's
//! `render_app` token injection). On a remote bind the daemon does not embed
//! it, so the global is the unreplaced placeholder; in that case the operator
//! supplies it via the URL fragment and we fall back to that.

use gloo_net::http::Request;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

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
