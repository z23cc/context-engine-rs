//! The placeholder Leptos shell for G1b: a dark sidebar + main area that, on
//! load, calls `runtime/info` and `runtime/tools/list` over `/rpc` and renders
//! the server name/version/protocol + tool count. This proves the Option-E
//! pipeline end to end; the real chat surface (the `session.*` command family)
//! is G2.

use crate::rpc::{daemon_token, rpc_call};
use leptos::prelude::*;
use nerve_proto::protocol::{RuntimeInfo, RuntimeToolsListResponse};
use serde_json::json;

/// What the bootstrap fetch produced: the parsed server info plus the tool
/// count, or a human-readable error string for the placeholder UI.
#[derive(Clone)]
struct Bootstrap {
    info: RuntimeInfo,
    tool_count: usize,
}

#[component]
pub fn App() -> impl IntoView {
    // `None` => still loading; `Some(Ok)` => loaded; `Some(Err)` => failed.
    let state = RwSignal::new(None::<Result<Bootstrap, String>>);

    // Kick the bootstrap fetch once, after mount. `spawn_local` runs the async
    // fetch on the browser's microtask queue without blocking render.
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            state.set(Some(load_bootstrap().await));
        });
    });

    view! {
        <div id="nerve-shell">
            <Sidebar state=state />
            <Main state=state />
        </div>
    }
}

/// Run the two bootstrap RPCs and fold them into a [`Bootstrap`] (or an error).
async fn load_bootstrap() -> Result<Bootstrap, String> {
    let token = daemon_token().ok_or_else(|| {
        "no daemon token — open the daemon's /app URL (or append #token=…)".to_string()
    })?;
    let info: RuntimeInfo = rpc_call(&token, "runtime/info", json!({})).await?;
    let tools: RuntimeToolsListResponse = rpc_call(&token, "runtime/tools/list", json!({})).await?;
    Ok(Bootstrap {
        info,
        tool_count: tools.tools.len(),
    })
}

#[component]
fn Sidebar(state: RwSignal<Option<Result<Bootstrap, String>>>) -> impl IntoView {
    view! {
        <aside class="sidebar">
            <div class="brand"><span class="spark">"◆"</span>" Nerve Console"</div>
            <div class="tagline">"Leptos · WASM · Protocol v4"</div>
            <h2>"Server"</h2>
            {move || match state.get() {
                Some(Ok(b)) => view! {
                    <div class="kv"><span class="k">"name"</span>
                        <span class="v">{b.info.server_info.name}</span></div>
                    <div class="kv"><span class="k">"version"</span>
                        <span class="v">{b.info.server_info.version}</span></div>
                    <div class="kv"><span class="k">"protocol"</span>
                        <span class="v">{b.info.protocol}</span></div>
                    <div class="kv"><span class="k">"protoVer"</span>
                        <span class="v">{b.info.protocol_version}</span></div>
                }.into_any(),
                _ => view! { <div class="placeholder">"connecting…"</div> }.into_any(),
            }}
        </aside>
    }
}

#[component]
fn Main(state: RwSignal<Option<Result<Bootstrap, String>>>) -> impl IntoView {
    view! {
        <main class="main">
            <h1>"Pipeline check"</h1>
            {move || status_badge(state.get())}
            <div class="card">
                <h3>"Runtime tools"</h3>
                {move || match state.get() {
                    Some(Ok(b)) => view! {
                        <div class="tools-count">{b.tool_count}
                            <span class="label">"tools registered"</span></div>
                    }.into_any(),
                    Some(Err(_)) => view! {
                        <div class="placeholder">"unavailable"</div>
                    }.into_any(),
                    None => view! { <div class="placeholder">"loading…"</div> }.into_any(),
                }}
            </div>
            <div class="card">
                <h3>"Next"</h3>
                <p class="placeholder">
                    "Chat surface (session.* over /rpc + /events) arrives in G2; \
                     Codex styling in G4."
                </p>
            </div>
        </main>
    }
}

/// The connection status pill reflecting the bootstrap fetch outcome.
fn status_badge(state: Option<Result<Bootstrap, String>>) -> impl IntoView {
    match state {
        Some(Ok(_)) => view! { <span class="status ok">"connected"</span> }.into_any(),
        Some(Err(err)) => {
            view! { <span class="status err">{format!("error: {err}")}</span> }.into_any()
        }
        None => view! { <span class="status loading">"loading…"</span> }.into_any(),
    }
}
