//! Chat surface + approvals + agent picker + a sidebar conversation list.
//!
//! The chat backend is the local agent CLIs (claude = Claude Code, codex =
//! Codex, gemini) over the DELEGATE path — not the in-process subscription
//! providers. Each conversation drives `delegate.start` (first message, the job
//! id becomes the live session id), then `delegate.steer` (follow-ups), with
//! `delegate.close` to end it. Replies stream as `DelegateProgress` text chunks
//! and a turn ends on `SessionIdle` (emitted by the daemon at delegate turn-end);
//! tool-permission requests surface as `ApprovalRequested` → the approval modal
//! (`session.respond`). Styling is a Codex-inspired native desktop surface.

// The Leptos root component compiles to one large declarative view tree; the
// workspace-wide `too_many_lines` deny is a poor fit for a view fn (the real
// logic lives in data.rs / rpc.rs / events.rs / the engine). Applied at module
// scope because the `#[component]` macro does not forward a fn-level allow to
// the function it generates.
#![allow(clippy::too_many_lines)]

use crate::context_view::ContextView;
use crate::events::route_event;
use crate::render::render_turn;
use crate::rpc::{cancel_job, daemon_token, new_job_id, open_events, start_job, start_job_with_id};
use crate::settings::SettingsModal;
use crate::sidebar::Sidebar;
use leptos::prelude::*;
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
}

#[derive(Clone)]
pub(crate) struct ToolCard {
    pub(crate) tool: String,
    pub(crate) ok: Option<bool>,
    pub(crate) output: String,
}

#[derive(Clone)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) text: String,
    pub(crate) reasoning: String,
    pub(crate) tools: Vec<ToolCard>,
    pub(crate) streaming: bool,
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
    pub(crate) fn assistant_streaming() -> Self {
        Self {
            role: Role::Assistant,
            text: String::new(),
            reasoning: String::new(),
            tools: Vec::new(),
            streaming: true,
        }
    }
}

/// One conversation in the sidebar list (in-memory for this browser session).
/// `session` is the delegate session id (the `delegate.start` job id); `turn_job`
/// is the in-flight turn's job id (start or steer), used to cancel/stop it.
#[derive(Clone)]
pub(crate) struct Chat {
    pub(crate) title: String,
    pub(crate) session: Option<String>,
    pub(crate) turn_job: Option<String>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) streaming: bool,
    /// Epoch-ms of the last activity (created / last message). Drives the rail's
    /// relative timestamp and recency sort.
    pub(crate) updated_ms: f64,
}

impl Chat {
    pub(crate) fn new() -> Self {
        Self {
            title: "New thread".into(),
            session: None,
            turn_job: None,
            turns: Vec::new(),
            streaming: false,
            updated_ms: js_sys::Date::now(),
        }
    }
}

/// A pending tool-permission decision (carries its own session_id so the reply
/// targets the right chat even if the user has switched conversations).
#[derive(Clone)]
pub(crate) struct ApprovalReq {
    pub(crate) session_id: String,
    pub(crate) request_id: String,
    pub(crate) tool: String,
    pub(crate) preview: String,
    pub(crate) tier: String,
}

#[component]
pub fn App() -> impl IntoView {
    let token = StoredValue::new(daemon_token());
    let chats = RwSignal::new(vec![Chat::new()]);
    let active = RwSignal::new(0usize);
    let input = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let approval = RwSignal::new(None::<ApprovalReq>);
    // Persisted defaults (theme + default agent/autonomy/model) seed the live
    // signals; an Effect below re-persists + re-applies the theme on any change.
    let saved = crate::settings::load();
    // The local CLI to drive (claude / codex) + the autonomy posture passed to
    // delegate.start (full = no prompts, edit, read_only = plan).
    let agent = RwSignal::new(saved.agent);
    let autonomy = RwSignal::new(saved.autonomy);
    // Optional model override passed to delegate.start (empty = the CLI's default).
    let model = RwSignal::new(saved.model);
    let theme = RwSignal::new(saved.theme);
    let search = RwSignal::new(String::new());
    let settings_open = RwSignal::new(false);
    let inspector_open = RwSignal::new(false);
    // Top-level surface: the delegate chat, or the Context builder.
    let mode = RwSignal::new("chat");
    // Multi-project: `workspaces` is every registered workspace `(name, root)`;
    // `workspace` is the ACTIVE one's name (the routing key threaded into every
    // tool call + delegate cwd + reveal). Switching projects re-points all of it.
    let workspaces = RwSignal::new(Vec::<(String, String)>::new());
    let workspace = RwSignal::new(String::new());
    let branch = RwSignal::new("—".to_string());
    // The active workspace's root path (for delegate cwd); "" until loaded. Read
    // only from event handlers, so untracked.
    let active_root = move || {
        let name = workspace.get_untracked();
        workspaces.with_untracked(|all| {
            all.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, root)| root.clone())
                .unwrap_or_default()
        })
    };

    Effect::new(move |_| {
        let Some(tok) = token.get_value() else {
            error.set(Some(
                "no daemon token — open the daemon's URL (or append #token=…)".into(),
            ));
            return;
        };
        let _ = open_events(&tok, move |event| route_event(event, chats, approval));
        leptos::task::spawn_local(async move {
            let list = crate::data::list_workspaces(&tok).await;
            if workspace.get_untracked().is_empty()
                && let Some((name, _)) = list.first()
            {
                workspace.set(name.clone());
            }
            workspaces.set(list);
        });
    });

    // Re-fetch the active workspace's branch whenever the selection changes.
    Effect::new(move |_| {
        let ws = workspace.get();
        if ws.is_empty() {
            return;
        }
        let Some(tok) = token.get_value() else { return };
        leptos::task::spawn_local(async move {
            branch.set(
                crate::data::fetch_branch(&tok, &ws)
                    .await
                    .unwrap_or_else(|| "—".into()),
            );
        });
    });

    // Persist defaults + (re)apply the theme whenever any of them changes.
    Effect::new(move |_| {
        let s = crate::settings::Settings {
            theme: theme.get(),
            agent: agent.get(),
            autonomy: autonomy.get(),
            model: model.get(),
        };
        crate::settings::apply_theme(&s.theme);
        crate::settings::save(&s);
    });

    // Keep the model valid for the selected agent (covers both the composer's
    // agent picker and the settings modal): a stale cross-agent model would send
    // e.g. a Claude model id to Codex.
    Effect::new(move |_| {
        let ag = agent.get();
        let m = model.get_untracked();
        let ok = m.is_empty()
            || crate::data::AGENT_MODELS
                .iter()
                .any(|(a, id, _)| *a == ag && *id == m);
        if !ok {
            model.set(String::new());
        }
    });

    // Whether the active chat is mid-turn (drives Send⇄Stop).
    let active_busy = move || chats.with(|cs| cs.get(active.get()).is_some_and(|c| c.streaming));

    let send = move || {
        let Some(tok) = token.get_value() else { return };
        let text = input.get_untracked().trim().to_string();
        if text.is_empty() || active_busy() {
            return;
        }
        input.set(String::new());
        error.set(None);
        let idx = active.get_untracked();
        let existing = chats.with_untracked(|cs| cs.get(idx).and_then(|c| c.session.clone()));
        let is_start = existing.is_none();
        // Install routing ids BEFORE the RPC: for delegate.start the daemon emits
        // DelegateProgress/ApprovalRequested keyed by this id concurrently with the
        // start round-trip, so the chat must already own it — otherwise turn-1 text
        // and approvals route to nobody. Also lets Stop target the turn immediately.
        let turn_id = new_job_id();
        chats.update(|cs| {
            if let Some(c) = cs.get_mut(idx) {
                if c.turns.is_empty() {
                    c.title = truncate_title(&text);
                }
                c.turns.push(Turn::user(text.clone()));
                c.turns.push(Turn::assistant_streaming());
                c.streaming = true;
                c.updated_ms = js_sys::Date::now();
                c.turn_job = Some(turn_id.clone());
                if is_start {
                    c.session = Some(turn_id.clone());
                }
            }
        });
        let (ag, au, md, root) = (
            agent.get_untracked(),
            autonomy.get_untracked(),
            model.get_untracked(),
            active_root(),
        );
        leptos::task::spawn_local(async move {
            let cmd = match &existing {
                Some(sid) => json!({"kind": "delegate.steer", "session_id": sid, "message": text}),
                None => {
                    let mut cmd = json!({"kind": "delegate.start", "agent": ag, "task": text, "autonomy": au});
                    if !md.is_empty() {
                        cmd["model"] = json!(md);
                    }
                    // Run the delegated CLI in the ACTIVE workspace's root.
                    if !root.is_empty() {
                        cmd["cwd"] = json!(root);
                    }
                    cmd
                }
            };
            if let Err(err) = start_job_with_id(&tok, &turn_id, cmd).await {
                // Roll back the optimistic session on a failed start.
                if is_start {
                    clear_session(chats, idx);
                }
                let verb = if is_start {
                    "delegate.start"
                } else {
                    "delegate.steer"
                };
                fail_chat(chats, idx, error, format!("{verb}: {err}"));
            }
        });
    };

    let stop = move || {
        let Some(tok) = token.get_value() else { return };
        let idx = active.get_untracked();
        let Some(job) = chats.with_untracked(|cs| cs.get(idx).and_then(|c| c.turn_job.clone()))
        else {
            return;
        };
        leptos::task::spawn_local(async move {
            let _ = cancel_job(&tok, &job).await;
        });
    };

    // Inspector (Plan/Files/Changes): Files + Changes load real data from the
    // daemon's snapshot-backed tools; Plan shows the active turn's tool calls.
    let inspector_tab = RwSignal::new("changes");
    let inspector_data = RwSignal::new(String::new());
    let load_tab = move |tab: &'static str| {
        inspector_tab.set(tab);
        if tab == "plan" {
            return;
        }
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        inspector_data.set("loading…".into());
        leptos::task::spawn_local(async move {
            let text = match tab {
                "files" => crate::data::fetch_file_tree(&tok, &ws).await,
                "changes" => crate::data::fetch_diff(&tok, &ws).await,
                _ => None,
            };
            inspector_data.set(text.unwrap_or_else(|| "—".into()));
        });
    };

    let toggle_inspector = move |_| {
        let opening = !inspector_open.get_untracked();
        inspector_open.set(opening);
        if opening {
            load_tab(inspector_tab.get_untracked());
        }
    };

    // Reveal the served workspace root in the OS file manager. Goes through the
    // runtime protocol (workspace.reveal), never native IPC — the daemon runs the
    // platform opener.
    let reveal = move || {
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        leptos::task::spawn_local(async move {
            let _ = start_job(&tok, json!({ "kind": "workspace.reveal", "workspace": ws })).await;
        });
    };

    // The composer: a large rounded box (textarea + an inline tool row) with
    // context pills beneath. Reused as the centered hero (empty state) and the
    // docked bar (active conversation). Copy closure → usable in both branches.
    let composer = move || {
        view! {
            <div class="composer-stack">
                <div class="composer-box">
                    <textarea
                        id="message"
                        name="message"
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
                        placeholder="Ask the agent to build…"
                    ></textarea>
                    <div class="composer-tools">
                        <button class="tool-btn" title="Attach — coming soon" disabled>"+"</button>
                        <select
                            class="access-pill"
                            title="Autonomy"
                            prop:value=move || autonomy.get()
                            on:change=move |ev| autonomy.set(event_target_value(&ev))
                        >
                            <option value="full">"Full access"</option>
                            <option value="edit">"Auto-edit"</option>
                            <option value="read_only">"Read-only"</option>
                        </select>
                        <span class="tool-spacer"></span>
                        <select
                            class="effort"
                            title="Model"
                            prop:value=move || model.get()
                            on:change=move |ev| model.set(event_target_value(&ev))
                        >
                            {move || {
                                let ag = agent.get();
                                crate::data::AGENT_MODELS.iter()
                                    .filter(move |(a, _, _)| *a == ag)
                                    .map(|(_, id, label)| view! { <option value=*id>{*label}</option> })
                                    .collect_view()
                            }}
                        </select>
                        {move || if active_busy() {
                            view! { <button class="send stop" title="Stop" on:click=move |_| stop()>"■"</button> }.into_any()
                        } else {
                            view! { <button class="send" title="Send" on:click=move |_| send()>"↑"</button> }.into_any()
                        }}
                    </div>
                </div>
                <div class="context-pills">
                    <button class="ctx-pill ctx-pill-act" title="Reveal in Finder"
                        on:click=move |_| reveal()>"📁 "{move || workspace.get()}</button>
                    <span class="ctx-pill">{move || crate::data::agent_label(&agent.get()).to_string()}</span>
                    <span class="ctx-pill">"⎇ "{move || branch.get()}</span>
                </div>
            </div>
        }
    };

    // Only flips when the active chat goes empty↔non-empty, so the composer is
    // not re-created (losing focus) on every streaming delta.
    let empty = Memo::new(move |_| {
        chats.with(|cs| {
            cs.get(active.get())
                .map(|c| c.turns.is_empty())
                .unwrap_or(true)
        })
    });

    view! {
        <div id="nerve-shell" class:with-inspector=move || inspector_open.get()>
            <Sidebar chats active input error token search workspace workspaces settings_open
                busy=Signal::derive(active_busy) />
            <main class="main chat">
                <div class="topbar">
                    <div class="picker">
                        <select
                            class="model-pill"
                            title="Agent CLI"
                            prop:value=move || agent.get()
                            on:change=move |ev| agent.set(event_target_value(&ev))
                        >
                            {crate::data::AGENTS.iter().map(|(id, label)| view! {
                                <option value=*id>{*label}</option>
                            }).collect_view()}
                        </select>
                        <button class="mode-toggle" title="Context builder"
                            on:click=move |_| mode.update(|m| *m = if *m == "context" { "chat" } else { "context" })>
                            {move || if mode.get() == "context" { "← Chat" } else { "Context" }}
                        </button>
                        <button class="icon-btn" title="Task pane" on:click=toggle_inspector>"⊞"</button>
                    </div>
                </div>
                {move || error.get().map(|e| view! { <div class="shell-error">{e}</div> })}
                {move || if mode.get() == "context" {
                    view! { <ContextView token=token workspace=workspace/> }.into_any()
                } else if empty.get() {
                    view! {
                        <div class="hero">
                            <h1 class="hero-title">"What should we build?"</h1>
                            <div class="hero-composer">{composer()}</div>
                            <div class="hero-chips">
                                <button class="hero-chip" on:click=move |_| input.set("Explain how this repository is organized.".into())>"Explain this repo"</button>
                                <button class="hero-chip" on:click=move |_| input.set("Make a step-by-step plan for the next change.".into())>"Make a plan"</button>
                                <button class="hero-chip" on:click=move |_| input.set("Find and fix a bug in this codebase.".into())>"Find a bug"</button>
                            </div>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="transcript">
                            {move || chats.with(|cs| cs.get(active.get()).map(|c| c.turns.clone()).unwrap_or_default())
                                .into_iter().map(render_turn).collect_view()}
                        </div>
                        <div class="composer-dock">{composer()}</div>
                    }.into_any()
                }}
            </main>
            {move || inspector_open.get().then(|| view! {
                <aside class="inspector">
                    <div class="inspector-head">
                        <span class="inspector-title">"Inspector"</span>
                        <span class="inspector-chip">"⎇ "{move || branch.get()}</span>
                    </div>
                    <div class="inspector-tabs">
                        <button class="inspector-tab" class:on=move || inspector_tab.get() == "plan"
                            on:click=move |_| load_tab("plan")>"Plan"</button>
                        <button class="inspector-tab" class:on=move || inspector_tab.get() == "files"
                            on:click=move |_| load_tab("files")>"Files"</button>
                        <button class="inspector-tab" class:on=move || inspector_tab.get() == "changes"
                            on:click=move |_| load_tab("changes")>"Changes"</button>
                    </div>
                    <div class="inspector-body">
                        {move || if inspector_tab.get() == "plan" {
                            let tools = chats.with(|cs| cs.get(active.get())
                                .and_then(|c| c.turns.last().map(|t| t.tools.clone()))
                                .unwrap_or_default());
                            if tools.is_empty() {
                                view! { <div class="plan-empty">"No tool activity in this thread yet."</div> }.into_any()
                            } else {
                                tools.into_iter().map(|t| view! {
                                    <div class="plan-step done"><span></span><p>{t.tool}</p></div>
                                }).collect_view().into_any()
                            }
                        } else {
                            view! { <pre class="inspector-pre">{move || inspector_data.get()}</pre> }.into_any()
                        }}
                    </div>
                </aside>
            })}
            {move || settings_open.get().then(|| view! {
                <SettingsModal open=settings_open theme=theme agent=agent autonomy=autonomy model=model />
            })}
            {move || approval.get().map(|req| view! {
                <ApprovalModal req=req token=token approval=approval />
            })}
        </div>
    }
}

#[component]
fn ApprovalModal(
    req: ApprovalReq,
    token: StoredValue<Option<String>>,
    approval: RwSignal<Option<ApprovalReq>>,
) -> impl IntoView {
    let decide = move |decision: &'static str| respond(token, approval, decision);
    view! {
        <div class="modal-scrim">
            <div class="modal">
                <div class="modal-head">
                    <span class="modal-title">"Allow "<b>{req.tool.clone()}</b></span>
                    <span class=format!("tier {}", req.tier.to_lowercase())>{req.tier.clone()}</span>
                </div>
                {(!req.preview.is_empty()).then(|| view! { <pre class="modal-preview">{req.preview.clone()}</pre> })}
                <div class="modal-actions">
                    <button class="btn allow" on:click=move |_| decide("allow")>"Allow"</button>
                    <button class="btn" on:click=move |_| decide("allow_always")>"Always"</button>
                    <button class="btn" on:click=move |_| decide("deny")>"Deny"</button>
                    <button class="btn danger" on:click=move |_| decide("deny_always")>"Deny always"</button>
                </div>
            </div>
        </div>
    }
}

/// Send a `session.respond` decision (delegate approvals share the same hub) and
/// clear the modal.
fn respond(
    token: StoredValue<Option<String>>,
    approval: RwSignal<Option<ApprovalReq>>,
    decision: &'static str,
) {
    let req = approval.get_untracked();
    approval.set(None);
    let (Some(tok), Some(req)) = (token.get_value(), req) else {
        return;
    };
    leptos::task::spawn_local(async move {
        let cmd = json!({
            "kind": "session.respond",
            "session_id": req.session_id,
            "request_id": req.request_id,
            "decision": decision,
        });
        let _ = start_job(&tok, cmd).await;
    });
}

/// Drop a chat's optimistic session id (rollback after a failed `delegate.start`).
fn clear_session(chats: RwSignal<Vec<Chat>>, idx: usize) {
    chats.update(|cs| {
        if let Some(c) = cs.get_mut(idx) {
            c.session = None;
        }
    });
}

/// Mark the chat's in-flight turn failed and surface the error.
fn fail_chat(
    chats: RwSignal<Vec<Chat>>,
    idx: usize,
    error: RwSignal<Option<String>>,
    message: String,
) {
    chats.update(|cs| {
        if let Some(c) = cs.get_mut(idx) {
            c.streaming = false;
            c.turn_job = None;
            if let Some(turn) = c.turns.last_mut() {
                turn.streaming = false;
            }
        }
    });
    error.set(Some(message));
}

/// A short sidebar title from the first user message.
fn truncate_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut title: String = line.chars().take(40).collect();
    if line.chars().count() > 40 {
        title.push('…');
    }
    if title.is_empty() {
        "New thread".into()
    } else {
        title
    }
}
