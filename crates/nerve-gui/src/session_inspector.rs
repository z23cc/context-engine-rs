//! Live CLI-agent dashboard for the right-side inspector.
//!
//! Each thread is one external CLI agent (delegate). This panel lists them all
//! straight from the in-memory thread list — no daemon round-trip — with status
//! (running / parked / idle) and Open + Stop controls, so several agents can be
//! managed side by side.

use crate::app::Chat;
use crate::rpc::cancel_job;
use leptos::prelude::*;

#[derive(Clone)]
struct AgentRow {
    index: usize,
    title: String,
    agent: String,
    live: bool,
    streaming: bool,
}

pub(crate) fn sessions_panel(
    token: StoredValue<Option<String>>,
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
) -> impl IntoView {
    let note = RwSignal::new(String::new());
    let open_agent = Callback::new(move |index: usize| {
        active.set(index);
        crate::dom::focus_message_input();
    });
    let stop_agent = Callback::new(move |index: usize| {
        stop_agent_turn(token, chats, note, index);
    });

    view! {
        <div id="sessions-panel" class="sessions-panel" role="tabpanel" aria-labelledby="inspector-tab-sessions" aria-describedby="agents-shortcuts" tabindex="-1">
            <section class="files-card sessions-card">
                <div class="files-card-head">
                    <span>"Agents"</span>
                    <div class="review-actions">
                        <span class="review-card-subtitle">"local CLI agents"</span>
                    </div>
                </div>
                <div id="agents-shortcuts" class="review-shortcuts" aria-label="Agents">
                    <span>"Every thread is one external CLI agent. Open switches to it; Stop cancels its running turn."</span>
                </div>
                {move || (!note.get().is_empty()).then(|| view! {
                    <span class="review-copy-note" role="status">{note.get()}</span>
                })}
            </section>
            {move || agents_body(chats, active, open_agent, stop_agent)}
        </div>
    }
}

fn agents_body(
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    open_agent: Callback<usize>,
    stop_agent: Callback<usize>,
) -> AnyView {
    let cur = active.get();
    let rows: Vec<AgentRow> = chats.with(|cs| {
        cs.iter()
            .enumerate()
            .map(|(index, c)| AgentRow {
                index,
                title: c.title.clone(),
                agent: c.agent.clone(),
                live: c.session.is_some(),
                streaming: c.streaming,
            })
            .collect()
    });
    if rows.is_empty() {
        return view! { <div class="plan-empty" role="status">"No agents yet."</div> }.into_any();
    }
    view! {
        <div class="session-list" role="list" aria-label="Agents">
            {rows.into_iter().map(|row| agent_row(row, cur, open_agent, stop_agent)).collect_view()}
        </div>
    }
    .into_any()
}

fn agent_row(
    row: AgentRow,
    cur: usize,
    open_agent: Callback<usize>,
    stop_agent: Callback<usize>,
) -> impl IntoView {
    let status = agent_status(row.streaming, row.live);
    let dot_class = format!("session-dot {status}");
    let meta = format!("{} · {}", crate::data::agent_label(&row.agent), status);
    let index = row.index;
    let is_active = index == cur;
    let title = row.title;
    let can_stop = row.streaming;
    let open_label = format!("Open agent thread {title}");
    view! {
        <article class="session-row" class:on=is_active role="listitem">
            <div class="session-main">
                <span class=dot_class aria-hidden="true"></span>
                <div>
                    <div class="session-title">{title}</div>
                    <div class="session-meta">{meta}</div>
                </div>
            </div>
            <div class="session-actions">
                <button class="review-action quiet" type="button" aria-label=open_label
                    on:click=move |_| open_agent.run(index)>"Open"</button>
                {if can_stop {
                    view! {
                        <button class="review-action quiet" type="button" aria-label="Stop agent turn"
                            on:click=move |_| stop_agent.run(index)>"Stop"</button>
                    }.into_any()
                } else {
                    view! { <span class="session-action-spacer" aria-hidden="true"></span> }.into_any()
                }}
            </div>
        </article>
    }
}

fn agent_status(streaming: bool, live: bool) -> &'static str {
    if streaming {
        "running"
    } else if live {
        "parked"
    } else {
        "idle"
    }
}

fn stop_agent_turn(
    token: StoredValue<Option<String>>,
    chats: RwSignal<Vec<Chat>>,
    note: RwSignal<String>,
    index: usize,
) {
    let Some(tok) = token.get_value() else {
        note.set("No daemon token; cannot stop agent.".into());
        return;
    };
    let job = chats.with_untracked(|cs| cs.get(index).and_then(|c| c.turn_job.clone()));
    let Some(job) = job else {
        note.set("That agent has no running turn.".into());
        return;
    };
    note.set("Stopping agent turn…".into());
    leptos::task::spawn_local(async move {
        let _ = cancel_job(&tok, &job).await;
        note.set("Stop requested.".into());
    });
}
