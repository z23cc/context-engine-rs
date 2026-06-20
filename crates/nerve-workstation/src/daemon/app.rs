//! Serving for the Leptos WASM frontend (G1b) at `/app`.
//!
//! This is a *new transport surface for the existing protocol*, never a new
//! protocol: the served bundle is a client of Protocol v4 that talks only to
//! `POST /rpc` + `GET /events`, exactly like the legacy `gui.html` at `/`. The
//! legacy GUI stays at `/` unchanged; this module adds the `/app` family.
//!
//! ## Embedding + the regen/drift story
//!
//! The built `dist/` artifacts are committed and `include_bytes!`'d here so the
//! daemon compiles + serves them with no `trunk` step at engine-build time (the
//! spike approach). The frontend is rebuilt with `trunk build` (cwd
//! `crates/nerve-gui`), which — via `Trunk.toml`'s `filehash = false` — emits
//! STABLE asset names (`nerve-gui.js`, `nerve-gui_bg.wasm`), so these include
//! paths never change. trunk also rewrites every asset href to be `/app/`-
//! prefixed (`Trunk.toml`'s `public_url = "/app/"`) so the served index resolves
//! its assets under the routes this module owns, and stamps an SRI `integrity`
//! hash on each — the committed index.html and wasm are therefore a matched
//! pair (the browser's SRI check ties them together).
//!
//! Long-term, CI should rebuild the frontend and drift-check the committed
//! `dist/` (mirroring the runtime-protocol schema drift discipline: regenerate,
//! fail on stale). Caveat: debug `wasm-bindgen`/`wasm-opt` output is **not**
//! byte-reproducible (the SRI hash shifts run to run), so a strict byte-diff
//! drift gate would be flaky — the gate should instead assert the committed
//! index.html's asset SRI matches the committed wasm (internal consistency), or
//! build release wasm with deterministic flags before diffing.

use super::http::{respond_asset, respond_html, respond_text};
use anyhow::Result;
use tiny_http::Request;

/// The committed Leptos `dist/` bundle, embedded at compile time. Paths are
/// stable because `Trunk.toml` disables file hashing (see the module docs).
const APP_INDEX_HTML: &str = include_str!("../../../nerve-gui/dist/index.html");
const APP_JS: &str = include_str!("../../../nerve-gui/dist/nerve-gui.js");
const APP_WASM: &[u8] = include_bytes!("../../../nerve-gui/dist/nerve-gui_bg.wasm");
const APP_CSS: &str = include_str!("../../../nerve-gui/dist/styles.css");

/// Token-injection marker prepended to the served `/app` index. The Leptos
/// frontend reads `window.__NERVE_DAEMON_TOKEN__`; trunk's generated HTML has no
/// placeholder of its own, so the daemon prepends a `<script>` that sets the
/// global the same way the legacy GUI's placeholder substitution does.
const APP_TOKEN_GLOBAL: &str = "__NERVE_DAEMON_TOKEN__";

/// Whether `path` is one of the `/app` routes this module owns. Kept as a free
/// function so the `http` dispatcher can branch without importing the asset set.
pub(super) fn is_app_path(path: &str) -> bool {
    matches!(path, "/app" | "/app/")
        || path
            .strip_prefix("/app/")
            .is_some_and(|asset| !asset.is_empty())
}

/// Serve a `/app` route: the index for `/app` + `/app/`, else the named asset.
/// `embed_token` is the per-run bearer token to bake into the index on a
/// loopback bind (the caller passes `HttpSecurity::embed_token()`), or `None`
/// on a remote bind so the page never carries it — mirroring the legacy GUI at
/// `/`. Unknown assets 404.
pub(super) fn serve_app(
    embed_token: Option<&str>,
    request: Request,
    path: &str,
    cors: Option<&str>,
) -> Result<()> {
    match path {
        "/app" | "/app/" => {
            let html = render_app(embed_token, APP_INDEX_HTML);
            respond_html(request, &html, cors)
        }
        _ => serve_asset(request, path, cors),
    }
}

/// Serve a hashed/stable asset under `/app/<name>` with the right Content-Type,
/// or 404 for an unknown name. Only the bundle's own assets are reachable — this
/// never reads the filesystem, so there is no path-traversal surface.
fn serve_asset(request: Request, path: &str, cors: Option<&str>) -> Result<()> {
    let name = path.strip_prefix("/app/").unwrap_or_default();
    match name {
        "nerve-gui.js" => respond_asset(request, APP_JS.as_bytes(), "text/javascript", cors),
        "nerve-gui_bg.wasm" => respond_asset(request, APP_WASM, "application/wasm", cors),
        "styles.css" => respond_asset(request, APP_CSS.as_bytes(), "text/css; charset=utf-8", cors),
        _ => respond_text(request, 404, "not found", cors),
    }
}

/// Inject the daemon token into the served index by inserting a `<script>` that
/// sets `window.__NERVE_DAEMON_TOKEN__`. `embed_token` is `None` on a remote
/// bind (the operator supplies it via the URL fragment), matching the legacy GUI.
fn render_app(embed_token: Option<&str>, template: &str) -> String {
    match embed_token {
        Some(token) => {
            let script = format!("<script>window.{APP_TOKEN_GLOBAL} = \"{token}\";</script>\n");
            inject_after_head(template, &script)
        }
        None => template.to_string(),
    }
}

/// Insert `snippet` immediately after the opening `<head>` tag (case-sensitive,
/// as trunk emits lowercase), falling back to prepending it if `<head>` is
/// absent so the global is always set before the WASM boot script runs.
fn inject_after_head(html: &str, snippet: &str) -> String {
    match html.find("<head>") {
        Some(idx) => {
            let cut = idx + "<head>".len();
            let mut out = String::with_capacity(html.len() + snippet.len());
            out.push_str(&html[..cut]);
            out.push('\n');
            out.push_str(snippet);
            out.push_str(&html[cut..]);
            out
        }
        None => format!("{snippet}{html}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_paths_are_recognized() {
        assert!(is_app_path("/app"));
        assert!(is_app_path("/app/"));
        assert!(is_app_path("/app/nerve-gui.js"));
        assert!(is_app_path("/app/nerve-gui_bg.wasm"));
        assert!(!is_app_path("/"));
        assert!(!is_app_path("/rpc"));
        assert!(!is_app_path("/application"));
    }

    #[test]
    fn embedded_bundle_is_present_and_is_a_protocol_client() {
        // The committed dist must actually be embedded (non-empty) and the app
        // must be a Protocol-v4 client: it loads the WASM glue and reads the
        // injected token global. (The /rpc call paths live in the .wasm.)
        assert!(APP_INDEX_HTML.contains("<!doctype html") || APP_INDEX_HTML.contains("<!DOCTYPE"));
        assert!(APP_INDEX_HTML.contains("nerve-gui.js"));
        assert!(!APP_JS.is_empty());
        assert!(!APP_WASM.is_empty());
        // The wasm bytes begin with the `\0asm` magic.
        assert_eq!(&APP_WASM[..4], b"\0asm");
    }

    #[test]
    fn render_app_injects_token_only_on_loopback() {
        // Loopback bind: the token is baked into <head>, before <body>.
        let html = render_app(Some("TOKEN123"), "<head>\n<body></body>");
        assert!(html.contains("window.__NERVE_DAEMON_TOKEN__ = \"TOKEN123\""));
        assert!(html.find("__NERVE_DAEMON_TOKEN__").unwrap() < html.find("<body>").unwrap());

        // Remote bind (no embed token): the page never carries the token.
        let remote = render_app(None, "<head>\n<body></body>");
        assert!(!remote.contains("TOKEN123"));
    }

    #[test]
    fn inject_after_head_falls_back_when_head_absent() {
        let out = inject_after_head("<html></html>", "<x/>");
        assert!(out.starts_with("<x/><html>"));
    }
}
