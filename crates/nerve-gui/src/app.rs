//! Chat surface + approvals + agent picker + a sidebar conversation list.
//!
//! The chat backend is, by product direction (2026-06-23), the **local CLI delegate**
//! path (`delegate.start` + `delegate.steer`) — driving an external agent CLI
//! (Claude Code / Codex / Gemini). The host-managed runtime session path
//! (`session.start` + `session.message`) is a **secondary, optional** engine kept
//! for headless/embedded use. Both route approvals through `session.respond`, so the
//! GUI keeps one permission surface. See docs/designs/architecture-north-star.md §1/§8.

// The Leptos root component compiles to one large declarative view tree; the
// workspace-wide `too_many_lines` deny is a poor fit for a view fn (the real
// logic lives in data.rs / rpc.rs / events.rs / the engine). Applied at module
// scope because the `#[component]` macro does not forward a fn-level allow to
// the function it generates.
#![allow(clippy::too_many_lines)]

use crate::approval::ApprovalModal;
use crate::chat_backend::{
    DelegateTurn, SessionTurn, active_turn_route, close_session_if_any, default_agent,
    default_chat_backend, send_delegate_turn, send_session_turn, session_id, stop_backend_turn,
};
use crate::chat_ops::{add_chat, clear_session, fail_chat, reset_chat};
use crate::command_palette::CommandPalette;
use crate::composer::Composer;
use crate::context_view::ContextView;
use crate::events::route_event;
use crate::hero_chips::HeroChips;
use crate::inspector::Inspector;
use crate::inspector_state::inspector_state;
use crate::render::render_turn;
use crate::rpc::{cancel_job, daemon_token, new_job_id, open_events, start_job};
use crate::settings::SettingsModal;
use crate::sidebar::Sidebar;
use crate::topbar::Topbar;
use leptos::prelude::*;
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Role {
    User,
    Assistant,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ToolCard {
    pub(crate) tool: String,
    pub(crate) ok: Option<bool>,
    #[serde(default)]
    pub(crate) input: String,
    pub(crate) output: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Turn {
    pub(crate) role: Role,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) reasoning: String,
    #[serde(default)]
    pub(crate) tools: Vec<ToolCard>,
    // Runtime-only: a restored turn is never mid-stream.
    #[serde(skip)]
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
/// `session` is the backend session id (`session.start` id or the delegate
/// live-session job id); `turn_job` is the in-flight turn job id, used to stop it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Chat {
    pub(crate) title: String,
    #[serde(default = "default_chat_backend")]
    pub(crate) backend: String,
    /// The external CLI agent bound to this thread (claude / codex / gemini),
    /// captured at first send. Drives the sidebar agent badge.
    #[serde(default = "default_agent")]
    pub(crate) agent: String,
    // Runtime-only (server-side session + in-flight job + streaming flag): never
    // persisted. A restored chat is offline history until the next message, which
    // opens a fresh backend session under the same thread.
    #[serde(skip)]
    pub(crate) session: Option<String>,
    #[serde(skip)]
    pub(crate) turn_job: Option<String>,
    #[serde(default)]
    pub(crate) turns: Vec<Turn>,
    #[serde(skip)]
    pub(crate) streaming: bool,
    /// Epoch-ms of the last activity (created / last message). Drives the rail's
    /// relative timestamp and recency sort.
    #[serde(default)]
    pub(crate) updated_ms: f64,
}

impl Chat {
    pub(crate) fn new_with_backend(backend: impl Into<String>) -> Self {
        Self {
            title: "New thread".into(),
            backend: backend.into(),
            agent: default_agent(),
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
    crate::clipboard::set_host_clipboard_token(token.get_value());
    let saved = crate::settings::load();
    // Restore persisted conversation history (offline: server-side sessions don't
    // survive a restart — a restored thread continues as a fresh session). If no
    // history exists, the first thread inherits the persisted backend default.
    let initial_backend = saved.chat_backend.clone();
    let chats = RwSignal::new({
        let restored = crate::settings::load_chats();
        if restored.is_empty() {
            vec![Chat::new_with_backend(initial_backend)]
        } else {
            restored
        }
    });
    let active = RwSignal::new(0usize);
    let input = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let approval = RwSignal::new(None::<ApprovalReq>);
    let chat_backend = RwSignal::new(saved.chat_backend);
    let agent = RwSignal::new(saved.agent);
    let autonomy = RwSignal::new(saved.autonomy);
    let model = RwSignal::new(saved.model);
    let runtime_provider = RwSignal::new(saved.runtime_provider);
    let runtime_model = RwSignal::new(saved.runtime_model);
    let theme = RwSignal::new(saved.theme);
    let theme_accent = RwSignal::new(saved.accent);
    let theme_bg = RwSignal::new(saved.bg);
    let theme_fg = RwSignal::new(saved.fg);
    let theme_font_ui = RwSignal::new(saved.font_ui);
    let theme_font_code = RwSignal::new(saved.font_code);
    let sidebar_vibrancy = RwSignal::new(saved.sidebar_vibrancy);
    let search = RwSignal::new(String::new());
    let settings_open = RwSignal::new(false);
    let palette_open = RwSignal::new(false);
    let inspector_open = RwSignal::new(false);
    // Top-level surface: chat, or the Context builder.
    let mode = RwSignal::new("chat");
    // Multi-project: active `workspace` routes tool calls, delegate cwd, and reveal.
    let workspaces = RwSignal::new(Vec::<(String, String)>::new());
    let workspace = RwSignal::new(String::new());
    let host_caps = RwSignal::new(None::<nerve_proto::HostCapabilities>);
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
        let workspace_tok = tok.clone();
        leptos::task::spawn_local(async move {
            let list = crate::data::list_workspaces(&workspace_tok).await;
            if workspace.get_untracked().is_empty()
                && let Some((name, _)) = list.first()
            {
                workspace.set(name.clone());
            }
            workspaces.set(list);
        });
        leptos::task::spawn_local(async move {
            host_caps.set(crate::data::fetch_host_capabilities(&tok).await.ok());
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
            accent: theme_accent.get(),
            bg: theme_bg.get(),
            fg: theme_fg.get(),
            font_ui: theme_font_ui.get(),
            font_code: theme_font_code.get(),
            sidebar_vibrancy: sidebar_vibrancy.get(),
            chat_backend: chat_backend.get(),
            agent: agent.get(),
            autonomy: autonomy.get(),
            model: model.get(),
            runtime_provider: runtime_provider.get(),
            runtime_model: runtime_model.get(),
        };
        let caps = host_caps.get();
        crate::settings::apply_theme(&s, caps.as_ref());
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

    // Persist conversation history whenever the chats settle (no stream in flight),
    // so a restart restores them. Tracks chats but only WRITES on a settled state —
    // during streaming it just runs the cheap any-streaming check.
    Effect::new(move |_| {
        chats.with(|cs| {
            if !cs.iter().any(|c| c.streaming) {
                crate::settings::save_chats(cs);
            }
        });
    });

    // Whether the active chat is mid-turn (drives Send⇄Stop).
    let active_busy = move || chats.with(|cs| cs.get(active.get()).is_some_and(|c| c.streaming));

    let send = move || {
        let Some(tok) = token.get_value() else { return };
        let text = input.get_untracked().trim().to_string();
        if text.is_empty() || active_busy() {
            return;
        }
        let idx = active.get_untracked();
        let existing = chats.with_untracked(|cs| cs.get(idx).and_then(|c| c.session.clone()));
        let backend = chats
            .with_untracked(|cs| cs.get(idx).map(|c| c.backend.clone()))
            .filter(|stored| existing.is_some() && !stored.is_empty())
            .unwrap_or_else(|| chat_backend.get_untracked());
        let is_start = existing.is_none();
        let (provider, session_model) = (
            runtime_provider.get_untracked(),
            runtime_model.get_untracked(),
        );
        if is_start
            && backend == "session"
            && (provider.trim().is_empty() || session_model.trim().is_empty())
        {
            error.set(Some(
                "Runtime Session needs provider and model in Settings.".into(),
            ));
            return;
        }
        input.set(String::new());
        error.set(None);
        // Install the turn job before the RPC so Stop can cancel the in-flight turn.
        // Delegate starts also use this id as the live session id; runtime sessions
        // get their daemon-generated id after `session.start` completes.
        let turn_id = new_job_id();
        chats.update(|cs| {
            if let Some(c) = cs.get_mut(idx) {
                if c.turns.is_empty() {
                    c.title = crate::data::truncate_title(&text);
                }
                c.turns.push(Turn::user(text.clone()));
                c.turns.push(Turn::assistant_streaming());
                c.streaming = true;
                c.updated_ms = js_sys::Date::now();
                c.turn_job = Some(turn_id.clone());
                if is_start {
                    c.backend = backend.clone();
                    c.agent = agent.get_untracked();
                    if backend == "delegate" {
                        c.session = Some(turn_id.clone());
                    }
                }
            }
        });
        let (ag, au, md, ws_name, root) = (
            agent.get_untracked(),
            autonomy.get_untracked(),
            model.get_untracked(),
            workspace.get_untracked(),
            active_root(),
        );
        leptos::task::spawn_local(async move {
            let result = if backend == "session" {
                send_session_turn(SessionTurn {
                    token: &tok,
                    chats,
                    idx,
                    existing,
                    turn_id: &turn_id,
                    text: &text,
                    provider: &provider,
                    model: &session_model,
                    workspace: &ws_name,
                })
                .await
            } else {
                send_delegate_turn(DelegateTurn {
                    token: &tok,
                    existing,
                    turn_id: &turn_id,
                    text: &text,
                    agent: &ag,
                    autonomy: &au,
                    model: &md,
                    root: &root,
                })
                .await
            };
            if let Err(err) = result {
                if is_start {
                    if backend == "session" {
                        close_session_if_any(&tok, session_id(chats, idx)).await;
                    }
                    clear_session(chats, idx);
                }
                fail_chat(chats, idx, error, err);
            }
        });
    };

    let stop = move || {
        let Some(tok) = token.get_value() else { return };
        let idx = active.get_untracked();
        let Some(route) = active_turn_route(chats, idx) else {
            return;
        };
        leptos::task::spawn_local(async move {
            stop_backend_turn(&tok, route).await;
        });
    };

    // Inspector: Tools is local thread state; Files/Changes/Review load daemon data.
    let inspector = inspector_state(token, workspace, inspector_open, mode);
    let inspector_tab = inspector.tab;
    let inspector_data = inspector.data;
    let load_tab = inspector.load_tab;
    let toggle_inspector = inspector.toggle;
    let palette_load_tab = load_tab;
    let open_inspector_tab = Callback::new(move |tab: &'static str| {
        inspector_open.set(true);
        palette_load_tab.run(tab);
    });
    let close_inspector = Callback::new(move |_| {
        inspector_open.set(false);
        crate::dom::focus_surface(mode.get_untracked());
    });

    // Reveal the served workspace root in the OS file manager. Goes through the
    // runtime protocol (workspace.reveal), never native IPC — the daemon runs the
    // platform opener.
    let reveal_workspace = Callback::new(move |_| {
        let Some(tok) = token.get_value() else { return };
        let ws = workspace.get_untracked();
        leptos::task::spawn_local(async move {
            let _ = start_job(&tok, json!({ "kind": "workspace.reveal", "workspace": ws })).await;
        });
    });

    let draft_review = Callback::new(move |prompt: String| {
        mode.set("chat");
        input.set(prompt);
        crate::dom::focus_message_input();
    });

    let palette_new_thread = Callback::new(move |_| {
        add_chat(
            chats,
            active,
            input,
            error,
            search,
            chat_backend.get_untracked(),
        );
        mode.set("chat");
    });

    let palette_clear_thread = Callback::new(move |_| {
        let idx = active.get_untracked();
        let (session, turn_job, backend) = chats.with_untracked(|cs| {
            cs.get(idx)
                .map(|c| (c.session.clone(), c.turn_job.clone(), c.backend.clone()))
                .unwrap_or_default()
        });
        if let Some(tok) = token.get_value() {
            leptos::task::spawn_local(async move {
                if let Some(job) = turn_job {
                    let _ = cancel_job(&tok, &job).await;
                }
                if let Some(sid) = session {
                    let kind = if backend == "session" {
                        "session.close"
                    } else {
                        "delegate.close"
                    };
                    let _ = start_job(&tok, json!({ "kind": kind, "session_id": sid })).await;
                }
            });
        }
        reset_chat(chats, idx, input, error, chat_backend.get_untracked());
        mode.set("chat");
    });

    let send_message = Callback::new(move |_| send());
    let stop_turn = Callback::new(move |_| stop());

    // Composer reused as centered hero and docked chat bar.
    let composer = move || {
        view! {
            <Composer
                agent=agent
                autonomy=autonomy
                branch=branch
                input=input
                mode=mode
                model=model
                palette_open=palette_open
                workspace=workspace
                busy=Signal::derive(active_busy)
                send=send_message
                stop=stop_turn
                reveal=reveal_workspace
            />
        }
    };

    // Avoid re-creating the composer on every streaming delta.
    let empty = Memo::new(move |_| {
        chats.with(|cs| {
            cs.get(active.get())
                .map(|c| c.turns.is_empty())
                .unwrap_or(true)
        })
    });
    let hero_review_tab = open_inspector_tab;
    let hero_tools_tab = open_inspector_tab;
    let open_review = Callback::new(move |_| hero_review_tab.run("review"));
    let open_tools = Callback::new(move |_| hero_tools_tab.run("plan"));

    let native_file_dialogs =
        Signal::derive(move || host_caps.get().is_some_and(|caps| caps.native_file_dialogs));

    crate::dom::install_chrome_guards(
        mode,
        inspector_open,
        inspector_tab,
        settings_open,
        palette_open,
    );

    view! {
        <div id="nerve-shell" aria-keyshortcuts="F6 Shift+F6" class:with-inspector=move || inspector_open.get()>
            <Sidebar chats active input error token search workspace workspaces settings_open mode inspector_open inspector_tab open_inspector_tab
                chat_backend=chat_backend
                native_file_dialogs=native_file_dialogs
                busy=Signal::derive(active_busy) />
            <main class="main chat">
                <Topbar
                    agent=agent
                    model=model
                    mode=mode
                    workspace=workspace
                    branch=branch
                    inspector_open=inspector_open
                    open_command_palette=Callback::new(move |_| palette_open.set(true))
                    toggle_inspector=toggle_inspector
                />
                {move || error.get().map(|e| view! { <div class="shell-error" role="alert">{e}</div> })}
                {move || if mode.get() == "context" {
                    view! {
                        <section id="surface-context" class="surface-panel" role="tabpanel" aria-labelledby="surface-tab-context" tabindex="-1">
                            <ContextView token=token workspace=workspace mode=mode native_file_dialogs=native_file_dialogs/>
                        </section>
                    }.into_any()
                } else if empty.get() {
                    view! {
                        <section id="surface-chat" class="surface-panel" role="tabpanel" aria-labelledby="surface-tab-chat" tabindex="-1">
                        <div class="hero">
                            <div class="hero-copy">
                                <h1 class="hero-title">"Work with code, context first"</h1>
                                <p class="hero-sub">{move || {
                                    let name = workspace.get();
                                    if name.is_empty() { "No workspace selected".to_string() } else { format!("{name} · chat, context, review, tools") }
                                }}</p>
                            </div>
                            <div class="hero-composer">{composer()}</div>
                            <HeroChips input=input mode=mode open_review=open_review open_tools=open_tools/>
                        </div>
                        </section>
                    }.into_any()
                } else {
                    view! {
                        <section id="surface-chat" class="surface-panel" role="tabpanel" aria-labelledby="surface-tab-chat" tabindex="-1">
                        <div class="transcript" role="log" aria-label="Conversation transcript" aria-live="polite" aria-relevant="additions text" aria-atomic="false">
                            {move || chats.with(|cs| cs.get(active.get()).map(|c| c.turns.clone()).unwrap_or_default())
                                .into_iter().map(render_turn).collect_view()}
                        </div>
                        <div class="composer-dock">{composer()}</div>
                        </section>
                    }.into_any()
                }}
            </main>
            {move || inspector_open.get().then(|| view! {
                <Inspector
                    branch=branch
                    tab=inspector_tab
                    data=inspector_data
                    token=token
                    chats=chats
                    active=active
                    load_tab=load_tab
                    close_inspector=close_inspector
                    draft_review=draft_review
                />
            })}
                <SettingsModal open=settings_open token=token theme=theme accent=theme_accent bg=theme_bg fg=theme_fg font_ui=theme_font_ui font_code=theme_font_code sidebar_vibrancy=sidebar_vibrancy agent=agent autonomy=autonomy model=model mode=mode />
            <CommandPalette open=palette_open mode=mode input=input token=token workspace=workspace chats=chats active_thread=active new_thread=palette_new_thread clear_thread=palette_clear_thread toggle_inspector=toggle_inspector open_inspector_tab=open_inspector_tab settings_open=settings_open native_file_dialogs=native_file_dialogs />
            {move || approval.get().map(|req| view! {
                <ApprovalModal req=req token=token approval=approval />
            })}
        </div>
    }
}
