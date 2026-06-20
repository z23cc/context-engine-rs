//! G2 chat surface: a multi-turn `session.*` chat over `/rpc` + `/events`.
//!
//! The composer starts a session on first send, streams the assistant turn from
//! the SSE `/events` stream (deserialized into the EXACT `nerve_proto` types —
//! the single protocol authority), renders markdown + reasoning + tool cards,
//! and offers a Stop button (`session.interrupt`) while a turn is in flight.
//! Original styling; the full Codex polish + sidebar/model picker are G3/G4.

use crate::rpc::{daemon_token, open_events, start_job};
use leptos::prelude::*;
use nerve_proto::{AgentEventKind, RuntimeEvent};
use serde_json::json;

/// Defaults for `session.start`. The model/provider picker is a G3 surface; for
/// now the chat starts a sensible default session and the engine can override.
const DEFAULT_PROVIDER: &str = "claude";
const DEFAULT_MODEL: &str = "claude-opus-4-8";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

/// A single tool call rendered inline in an assistant turn.
#[derive(Clone)]
struct ToolCard {
    tool: String,
    ok: Option<bool>,
    output: String,
}

/// One conversation turn. Assistant turns stream: text/reasoning/tools fill in
/// as `SessionAgent` events arrive; `streaming` clears on idle/interrupt.
#[derive(Clone)]
struct Turn {
    role: Role,
    text: String,
    reasoning: String,
    tools: Vec<ToolCard>,
    streaming: bool,
}

impl Turn {
    fn user(text: String) -> Self {
        Self {
            role: Role::User,
            text,
            reasoning: String::new(),
            tools: Vec::new(),
            streaming: false,
        }
    }
    fn assistant_streaming() -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            streaming: true,
        }
    }
}

#[component]
pub fn App() -> impl IntoView {
    // `StoredValue` so the (non-Copy) token can be captured by multiple handlers.
    let token = StoredValue::new(daemon_token());
    let conversation = RwSignal::new(Vec::<Turn>::new());
    let session = RwSignal::new(None::<String>);
    let streaming = RwSignal::new(false);
    let input = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);

    // Open the SSE stream once (app lifetime); route events into the signals.
    Effect::new(move |_| {
        if let Some(tok) = token.get_value() {
            let _ = open_events(&tok, move |event| {
                route_event(event, session, conversation, streaming);
            });
        } else {
            error.set(Some(
                "no daemon token — open the daemon's /app URL (or append #token=…)".into(),
            ));
        }
    });

    let send = move || {
        let Some(tok) = token.get_value() else { return };
        let text = input.get_untracked().trim().to_string();
        if text.is_empty() || streaming.get_untracked() {
            return;
        }
        input.set(String::new());
        error.set(None);
        conversation.update(|c| {
            c.push(Turn::user(text.clone()));
            c.push(Turn::assistant_streaming());
        });
        streaming.set(true);
        leptos::task::spawn_local(async move {
            let session_id = match ensure_session(&tok, session).await {
                Ok(id) => id,
                Err(err) => return finish_with_error(conversation, streaming, error, err),
            };
            let cmd = json!({"kind": "session.message", "session_id": session_id, "text": text});
            if let Err(err) = start_job(&tok, cmd).await {
                finish_with_error(conversation, streaming, error, format!("session.message: {err}"));
            }
        });
    };

    let stop = move || {
        let Some(tok) = token.get_value() else { return };
        let Some(session_id) = session.get_untracked() else {
            return;
        };
        leptos::task::spawn_local(async move {
            let _ = start_job(&tok, json!({"kind": "session.interrupt", "session_id": session_id}))
                .await;
        });
    };

    let new_chat = move |_| {
        conversation.set(Vec::new());
        session.set(None);
        streaming.set(false);
        error.set(None);
    };

    view! {
        <div id="nerve-shell">
            <aside class="sidebar">
                <div class="brand"><span class="spark">"◆"</span>" Nerve Console"</div>
                <div class="tagline">"chat · session.* · protocol v4"</div>
                <button class="newchat" on:click=new_chat>"＋ New chat"</button>
                <div class="spacer"></div>
                <div class="status-row">
                    {move || match (session.get(), streaming.get()) {
                        (Some(_), true) => view! { <span class="dot busy"></span>"streaming" }.into_any(),
                        (Some(_), false) => view! { <span class="dot ok"></span>"session live" }.into_any(),
                        (None, _) => view! { <span class="dot idle"></span>"no session" }.into_any(),
                    }}
                </div>
            </aside>
            <main class="main chat">
                <div class="transcript">
                    {move || {
                        let turns = conversation.get();
                        if turns.is_empty() {
                            view! { <div class="empty">
                                <div class="empty-spark">"◆"</div>
                                <div class="empty-title">"Nerve Console"</div>
                                <div class="empty-sub">"Ask anything — the agent runs over the runtime protocol."</div>
                            </div> }.into_any()
                        } else {
                            turns.into_iter().map(render_turn).collect_view().into_any()
                        }
                    }}
                    {move || error.get().map(|e| view! { <div class="turn-error">{e}</div> })}
                </div>
                <div class="composer">
                    <textarea
                        class="input"
                        rows="1"
                        prop:value=move || input.get()
                        on:input=move |ev| input.set(event_target_value(&ev))
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" && !ev.shift_key() {
                                ev.prevent_default();
                                send();
                            }
                        }
                        placeholder="Message Nerve…  (Enter to send · Shift+Enter for newline)"
                    ></textarea>
                    {move || if streaming.get() {
                        view! { <button class="send stop" title="Stop" on:click=move |_| stop()>"■"</button> }.into_any()
                    } else {
                        view! { <button class="send" title="Send" on:click=move |_| send()>"↑"</button> }.into_any()
                    }}
                </div>
            </main>
        </div>
    }
}

/// Return the live session id, starting one (`session.start`) if needed.
async fn ensure_session(token: &str, session: RwSignal<Option<String>>) -> Result<String, String> {
    if let Some(id) = session.get_untracked() {
        return Ok(id);
    }
    let cmd = json!({"kind": "session.start", "provider": DEFAULT_PROVIDER, "model": DEFAULT_MODEL});
    let result = start_job(token, cmd)
        .await
        .map_err(|err| format!("session.start: {err}"))?;
    let id = result
        .get("job")
        .and_then(|j| j.get("result"))
        .and_then(|r| r.get("session_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "session.start returned no session_id".to_string())?;
    session.set(Some(id.clone()));
    Ok(id)
}

/// Mark the in-flight turn failed and surface the error.
fn finish_with_error(
    conversation: RwSignal<Vec<Turn>>,
    streaming: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    message: String,
) {
    streaming.set(false);
    conversation.update(|c| {
        if let Some(turn) = c.last_mut() {
            if turn.role == Role::Assistant {
                turn.streaming = false;
            }
        }
    });
    error.set(Some(message));
}

/// Route one `RuntimeEvent` from the SSE stream into the conversation, scoped to
/// the active session.
fn route_event(
    event: RuntimeEvent,
    session: RwSignal<Option<String>>,
    conversation: RwSignal<Vec<Turn>>,
    streaming: RwSignal<bool>,
) {
    let current = session.get_untracked();
    match event {
        RuntimeEvent::SessionStarted { session_id } => {
            if current.is_none() {
                session.set(Some(session_id));
            }
        }
        RuntimeEvent::SessionIdle { session_id } => {
            if current.as_deref() == Some(session_id.as_str()) {
                streaming.set(false);
                conversation.update(|c| {
                    if let Some(turn) = c.last_mut() {
                        turn.streaming = false;
                    }
                });
            }
        }
        RuntimeEvent::SessionClosed { session_id } => {
            if current.as_deref() == Some(session_id.as_str()) {
                streaming.set(false);
            }
        }
        RuntimeEvent::SessionAgent { session_id, event } => {
            if current.as_deref() == Some(session_id.as_str()) {
                apply_agent_event(event, conversation, streaming);
            }
        }
        _ => {}
    }
}

/// Fold a single `AgentEventKind` into the current (streaming) assistant turn.
fn apply_agent_event(
    event: AgentEventKind,
    conversation: RwSignal<Vec<Turn>>,
    streaming: RwSignal<bool>,
) {
    let mut interrupted = false;
    conversation.update(|c| {
        let needs_turn = !matches!(c.last(), Some(t) if t.role == Role::Assistant && t.streaming);
        if needs_turn {
            c.push(Turn::assistant_streaming());
        }
        let Some(turn) = c.last_mut() else { return };
        match event {
            AgentEventKind::Message { text } => turn.text.push_str(&text),
            AgentEventKind::Reasoning { text } => turn.reasoning.push_str(&text),
            AgentEventKind::ToolStarted { tool, .. } => turn.tools.push(ToolCard {
                tool,
                ok: None,
                output: String::new(),
            }),
            AgentEventKind::ToolFinished { tool, ok, output } => {
                match turn
                    .tools
                    .iter_mut()
                    .rev()
                    .find(|card| card.tool == tool && card.ok.is_none())
                {
                    Some(card) => {
                        card.ok = Some(ok);
                        card.output = output;
                    }
                    None => turn.tools.push(ToolCard {
                        tool,
                        ok: Some(ok),
                        output,
                    }),
                }
            }
            AgentEventKind::Interrupted { .. } => {
                turn.streaming = false;
                interrupted = true;
            }
            AgentEventKind::TurnStarted { .. } | AgentEventKind::Usage { .. } => {}
        }
    });
    if interrupted {
        streaming.set(false);
    }
}

/// Render one conversation turn.
fn render_turn(turn: Turn) -> AnyView {
    match turn.role {
        Role::User => view! {
            <div class="turn user"><div class="bubble">{turn.text}</div></div>
        }
        .into_any(),
        Role::Assistant => {
            let html = markdown_to_html(&turn.text);
            let reasoning = turn.reasoning.clone();
            let tools = turn.tools.clone();
            let streaming = turn.streaming;
            view! {
                <div class="turn assistant">
                    {(!reasoning.is_empty()).then(|| view! {
                        <details class="reasoning">
                            <summary>"reasoning"</summary>
                            <pre>{reasoning}</pre>
                        </details>
                    })}
                    <div class="md" inner_html=html></div>
                    {tools.into_iter().map(render_tool).collect_view()}
                    {streaming.then(|| view! { <span class="cursor">"▋"</span> })}
                </div>
            }
            .into_any()
        }
    }
}

/// Render one tool-call card (run / ok / error).
fn render_tool(card: ToolCard) -> AnyView {
    let status = match card.ok {
        None => "run",
        Some(true) => "ok",
        Some(false) => "err",
    };
    let badge = match card.ok {
        None => "running",
        Some(true) => "ok",
        Some(false) => "error",
    };
    view! {
        <div class=format!("tool {status}")>
            <div class="tool-head">
                <span class="tool-name">{card.tool}</span>
                <span class="tool-badge">{badge}</span>
            </div>
            {(!card.output.is_empty()).then(|| view! { <pre class="tool-out">{card.output}</pre> })}
        </div>
    }
    .into_any()
}

/// Render markdown to sanitized HTML: raw inline/block HTML in model output is
/// downgraded to escaped text (no script injection), code is escaped by the
/// writer. Loopback-only, but defense-in-depth regardless.
fn markdown_to_html(src: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(src, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}
