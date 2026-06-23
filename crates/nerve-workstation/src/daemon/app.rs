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

/// Serve the Leptos app index (the token-injected `index.html`). Used for both
/// `GET /` (the primary GUI since the G4 flip) and `/app` (kept for compat). The
/// index references its assets at absolute `/app/…` paths (trunk `public_url`),
/// which [`serve_asset`] owns, so serving the index at `/` resolves correctly.
pub(super) fn serve_index(
    embed_token: Option<&str>,
    request: Request,
    cors: Option<&str>,
) -> Result<()> {
    let html = render_app(embed_token, APP_INDEX_HTML);
    respond_html(request, &html, cors)
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

    fn bytes_contain(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    fn assert_gui_auth_lease_source_contract(settings: &str, auth: &str, styles: &str) {
        assert!(settings.contains("Broker OAuth"));
        assert!(settings.contains("BrokerOAuthControls token=token"));
        assert!(auth.contains("auth.status"));
        assert!(auth.contains("Check status"));
        assert!(auth.contains("Ok(result) => format_auth_status(&result)"));
        assert!(auth.contains("auth.start"));
        assert!(auth.contains("Start browser login"));
        assert!(auth.contains("auth.complete"));
        assert!(auth.contains("Complete login"));
        assert!(auth.contains("Paste callback URL or code"));
        assert!(auth.contains("Open authorization URL"));
        assert!(
            auth.contains(
                "disabled=move || auth_busy.get() || login_busy.get() || lease_busy.get()"
            )
        );
        assert!(auth.contains("Device-code login"));
        assert!(auth.contains("Runtime bearer: not exposed through runtime"));
        assert!(auth.contains("auth.lease"));
        assert!(auth.contains("\"include_token\": false"));
        assert!(auth.contains("Access token: not returned to Web GUI"));
        assert!(auth.contains("Access token: stored by daemon; not returned to Web GUI"));
        assert!(auth.contains("role=\"status\""));
        assert!(auth.contains("aria-live=\"polite\""));
        assert!(auth.contains("lease-status"));
        assert!(auth.contains("auth-status"));
        assert!(auth.contains("login-status"));
        assert!(styles.contains(".lease-status"));
        assert!(styles.contains(".auth-status"));
        assert!(styles.contains(".login-status"));
        assert!(styles.contains(".auth-callback"));
        assert!(styles.contains(".auth-link"));
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

    fn assert_gui_composer_source_contract(app_source: &str, composer: &str, hero_chips: &str) {
        // Composer shell (now in composer.rs): the Local-workspace execution pill
        // and the per-thread agent picker.
        assert!(composer.contains("composer-modes"));
        assert!(composer.contains("Local workspace"));
        assert!(composer.contains("Execution target"));
        assert!(composer.contains("Describe a task"));
        assert!(composer.contains("Agent CLI"));
        // Empty-state hero + quick-start cards.
        assert!(app_source.contains("Work with code, context first"));
        assert!(app_source.contains("hero-sub"));
        assert!(app_source.contains("No workspace selected"));
        assert!(app_source.contains("class:with-inspector"));
        assert!(app_source.contains("HeroChips"));
        assert!(hero_chips.contains("aria-label=\"Quick start\""));
        assert!(hero_chips.contains("Plan a change"));
        assert!(hero_chips.contains("Build context"));
        assert!(hero_chips.contains("chip-icon"));
    }

    fn assert_gui_chrome_source_contract(
        topbar: &str,
        sidebar: &str,
        render: &str,
        inspector: &str,
    ) {
        assert!(topbar.contains("model-menu"));
        assert!(topbar.contains("model-popover"));
        assert!(topbar.contains("Model picker"));
        assert!(topbar.contains("agent.set(event_target_value(&ev))"));
        assert!(topbar.contains("model.set(event_target_value(&ev))"));
        assert!(sidebar.contains("aria-label=\"Workspace navigation\""));
        assert!(sidebar.contains(">\"Threads\"</span>"));
        assert!(sidebar.contains(">\"Context\"</span>"));
        assert!(sidebar.contains("thread-rail-wrap"));
        assert!(sidebar.contains("rail-sub"));
        assert!(sidebar.contains("nav-svg"));
        assert!(render.contains("Thought for this step"));
        assert!(render.contains("tool-dot"));
        assert!(!render.contains("tool-badge"));
        // Inspector tabs, incl. the Agents dashboard.
        assert!(inspector.contains(">\"Tools\"</button>"));
        assert!(inspector.contains(">\"Agents\"</button>"));
        assert!(inspector.contains(">\"Changes\"</button>"));
    }

    fn assert_gui_settings_source_contract(
        settings: &str,
        settings_auth: &str,
        index: &str,
        styles: &str,
    ) {
        assert!(settings.contains("accent: RwSignal<String>"));
        assert!(settings.contains("set_var(style, \"--accent\""));
        assert!(settings.contains("set_var(&style, \"--font-code\""));
        assert!(settings.contains("token_input(font_ui"));
        assert!(settings.contains("font_code"));
        assert!(settings.contains("sidebar_vibrancy: RwSignal<bool>"));
        assert!(settings.contains("bool_toggle(sidebar_vibrancy"));
        assert!(settings.contains("Vibrant sidebar"));
        assert_gui_auth_lease_source_contract(settings, settings_auth, styles);
        assert!(index.contains("font_code: \"--font-code\""));
        assert!(index.contains("el.style.setProperty(vars[key], value)"));
        assert!(index.contains("data-vibrancy"));
        assert!(index.contains("sidebar_vibrancy"));
    }

    fn assert_gui_style_source_contract(styles: &str) {
        assert!(styles.contains("--bg: #f7f6f3;"));
        assert!(styles.contains("--border: #e1dfd8;"));
        assert!(styles.contains("--sidebar-width: 260px;"));
        assert!(styles.contains("--r-card: 12px;"));
        assert!(styles.contains("font-size: var(--fs-label);"));
        assert!(styles.contains("letter-spacing: 0.04em;"));
        assert!(styles.contains("text-transform: uppercase;"));
        assert!(styles.contains("background: var(--inspector);"));
        assert!(styles.contains(".thread-rail-wrap"));
        assert!(styles.contains(".rail-sub"));
        assert!(styles.contains("stroke-linecap: round;"));
        assert!(styles.contains("gap: 28px;"));
        assert!(styles.contains("border-radius: var(--r-card);"));
        assert!(styles.contains(".tool-dot"));
        assert!(styles.contains("font-family: var(--font-code);"));
        assert!(styles.contains("#nerve-shell.with-inspector"));
        assert!(styles.contains(".inspector-tab"));
        assert!(styles.contains(".plan-step"));
        assert!(styles.contains(".set-input"));
        assert!(styles.contains(".set-toggle"));
        assert!(styles.contains(":root[data-vibrancy='sidebar'] .sidebar"));
        assert!(styles.contains("backdrop-filter: saturate(180%) blur(20px);"));
    }

    fn assert_gui_dist_contract() {
        assert!(APP_INDEX_HTML.contains("font_code: \"--font-code\""));
        assert!(APP_INDEX_HTML.contains("el.style.setProperty(vars[key], value)"));
        assert!(APP_INDEX_HTML.contains("data-vibrancy"));
        assert!(APP_CSS.contains("--bg: #f7f6f3;"));
        assert!(APP_CSS.contains("--sidebar-width: 260px;"));
        assert!(APP_CSS.contains("background: var(--inspector);"));
        assert!(APP_CSS.contains("text-transform: uppercase;"));
        assert!(APP_CSS.contains(".thread-rail-wrap"));
        assert!(APP_CSS.contains(".rail-sub"));
        assert!(APP_CSS.contains(".tool-dot"));
        assert!(APP_CSS.contains("gap: 28px;"));
        assert!(APP_CSS.contains("#nerve-shell.with-inspector"));
        assert!(APP_CSS.contains(".inspector-tab"));
        assert!(APP_CSS.contains(".set-input"));
        assert!(APP_CSS.contains(".set-toggle"));
        assert!(APP_CSS.contains(".lease-status"));
        assert!(APP_CSS.contains(".auth-status"));
        assert!(APP_CSS.contains(":root[data-vibrancy='sidebar'] .sidebar"));
        assert!(APP_CSS.contains("backdrop-filter: saturate(180%) blur(20px);"));
    }

    #[test]
    fn gui_source_declares_codex_composer_modes() {
        // The source surface should keep the Codex-style execution-mode affordance
        // close to the composer. Worktree/Cloud are deliberately disabled until
        // their execution semantics exist, but the visual shell is present.
        let source = include_str!("../../../nerve-gui/src/app.rs");
        let composer = include_str!("../../../nerve-gui/src/composer.rs");
        let hero_chips = include_str!("../../../nerve-gui/src/hero_chips.rs");
        let topbar = include_str!("../../../nerve-gui/src/topbar.rs");
        let sidebar = include_str!("../../../nerve-gui/src/sidebar.rs");
        let render = include_str!("../../../nerve-gui/src/render.rs");
        let inspector = include_str!("../../../nerve-gui/src/inspector.rs");
        let settings = include_str!("../../../nerve-gui/src/settings.rs");
        let settings_auth = include_str!("../../../nerve-gui/src/settings_auth.rs");
        let index = include_str!("../../../nerve-gui/index.html");
        let styles = include_str!("../../../nerve-gui/styles.css");

        assert_gui_composer_source_contract(source, composer, hero_chips);
        assert_gui_chrome_source_contract(topbar, sidebar, render, inspector);
        assert_gui_settings_source_contract(settings, settings_auth, index, styles);
        assert_gui_style_source_contract(styles);
        assert_gui_dist_contract();
    }

    #[test]
    fn embedded_bundle_exposes_sticky_approval_decisions() {
        // The committed dist must stay in sync with the source approval modal:
        // sticky allow/deny decisions are a Protocol-v4 UX feature, not TUI-only.
        assert!(bytes_contain(APP_WASM, "Allow for session"));
        assert!(bytes_contain(APP_WASM, "Always deny"));
        assert!(bytes_contain(APP_WASM, "allow_always"));
        assert!(bytes_contain(APP_WASM, "deny_always"));
    }

    #[test]
    fn embedded_bundle_exposes_codex_composer_modes() {
        // Dist-sync guard for the primary `/app` bundle: the daemon-served WASM
        // must carry the visible composer mode shell, not just the Rust source.
        // Use distinctive UI strings instead of generic words like "Local".
        assert!(bytes_contain(APP_WASM, "Local workspace"));
        assert!(bytes_contain(APP_WASM, "Describe a task"));
        assert!(bytes_contain(APP_WASM, "No workspace selected"));
        assert!(bytes_contain(APP_WASM, "Quick start"));
        assert!(bytes_contain(APP_WASM, "Plan a change"));
        assert!(bytes_contain(APP_WASM, "Build context"));
        assert!(bytes_contain(APP_WASM, "Agent CLI"));
        assert!(bytes_contain(APP_WASM, "Model picker"));
        assert!(bytes_contain(APP_WASM, "Agents"));
        assert!(bytes_contain(APP_WASM, "Threads"));
        assert!(bytes_contain(APP_WASM, "Workspace navigation"));
        assert!(bytes_contain(APP_WASM, "Thought for this step"));
        assert!(bytes_contain(APP_WASM, "Inspector"));
        assert!(bytes_contain(APP_WASM, "Files"));
        assert!(bytes_contain(APP_WASM, "Changes"));
        assert!(bytes_contain(APP_WASM, "No tool activity"));
        assert!(bytes_contain(APP_WASM, "Accent"));
        assert!(bytes_contain(APP_WASM, "UI font"));
        assert!(bytes_contain(APP_WASM, "Code font"));
        assert!(bytes_contain(APP_WASM, "Sidebar material"));
        assert!(bytes_contain(APP_WASM, "Vibrant sidebar"));
        assert!(bytes_contain(APP_WASM, "Broker OAuth"));
        assert!(bytes_contain(APP_WASM, "Check status"));
        assert!(bytes_contain(APP_WASM, "Start browser login"));
        assert!(bytes_contain(APP_WASM, "Complete login"));
        assert!(bytes_contain(APP_WASM, "Paste callback URL or code"));
        assert!(bytes_contain(APP_WASM, "Open authorization URL"));
        assert!(bytes_contain(APP_WASM, "Device-code login"));
        assert!(bytes_contain(APP_WASM, "Runtime bearer:"));
        assert!(bytes_contain(APP_WASM, "not exposed through runtime"));
        assert!(bytes_contain(APP_WASM, "Check lease"));
        assert!(bytes_contain(APP_WASM, "Force refresh lease"));
        assert!(bytes_contain(
            APP_WASM,
            "Access token: not returned to Web GUI"
        ));
        assert!(bytes_contain(
            APP_WASM,
            "Access token: stored by daemon; not returned to Web GUI"
        ));
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
