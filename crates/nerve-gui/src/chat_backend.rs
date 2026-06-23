//! Chat backend adapters for the GUI composer.
//!
//! The GUI runs a turn primarily through the local delegate CLI path
//! (`delegate.start` + `delegate.steer`) — the product-facing way to drive an
//! external agent CLI. The runtime session API (`session.start` + `session.message`)
//! is a secondary, optional own-engine path. Keeping this outside `app.rs` preserves
//! the root component as wiring, not transport logic.

use crate::app::Chat;
use crate::rpc::{cancel_job, start_job, start_job_await, start_job_with_id};
use leptos::prelude::*;
use serde_json::json;

pub(crate) fn default_chat_backend() -> String {
    "delegate".into()
}

/// Default CLI agent bound to a fresh thread until the first send rebinds it.
pub(crate) fn default_agent() -> String {
    "claude".into()
}

pub(crate) struct TurnRoute {
    job: String,
    session: Option<String>,
    backend: String,
}

pub(crate) fn session_id(chats: RwSignal<Vec<Chat>>, idx: usize) -> Option<String> {
    chats.with_untracked(|cs| cs.get(idx).and_then(|chat| chat.session.clone()))
}

pub(crate) fn active_turn_route(chats: RwSignal<Vec<Chat>>, idx: usize) -> Option<TurnRoute> {
    chats.with_untracked(|cs| {
        cs.get(idx).and_then(|chat| {
            chat.turn_job.clone().map(|job| TurnRoute {
                job,
                session: chat.session.clone(),
                backend: chat.backend.clone(),
            })
        })
    })
}

pub(crate) async fn close_session_if_any(token: &str, session_id: Option<String>) {
    if let Some(session_id) = session_id {
        let _ = start_job(
            token,
            json!({ "kind": "session.close", "session_id": session_id }),
        )
        .await;
    }
}

pub(crate) async fn stop_backend_turn(token: &str, route: TurnRoute) {
    if route.backend == "session"
        && let Some(session_id) = route.session
    {
        let _ = start_job(
            token,
            json!({ "kind": "session.interrupt", "session_id": session_id }),
        )
        .await;
    }
    let _ = cancel_job(token, &route.job).await;
}

pub(crate) struct SessionTurn<'a> {
    pub(crate) token: &'a str,
    pub(crate) chats: RwSignal<Vec<Chat>>,
    pub(crate) idx: usize,
    pub(crate) existing: Option<String>,
    pub(crate) turn_id: &'a str,
    pub(crate) text: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) workspace: &'a str,
}

pub(crate) async fn send_session_turn(turn: SessionTurn<'_>) -> Result<(), String> {
    let session_id = match turn.existing {
        Some(session_id) => session_id,
        None => {
            start_session(
                turn.token,
                turn.chats,
                turn.idx,
                turn.provider,
                turn.model,
                turn.workspace,
            )
            .await?
        }
    };
    start_job_with_id(
        turn.token,
        turn.turn_id,
        json!({ "kind": "session.message", "session_id": session_id, "text": turn.text }),
    )
    .await
    .map_err(|err| format!("session.message: {err}"))
}

async fn start_session(
    token: &str,
    chats: RwSignal<Vec<Chat>>,
    idx: usize,
    provider: &str,
    model: &str,
    workspace: &str,
) -> Result<String, String> {
    let mut command = json!({
        "kind": "session.start",
        "provider": provider,
        "model": model,
    });
    if !workspace.is_empty() {
        command["workspace"] = json!(workspace);
    }
    let result = start_job_await(token, command)
        .await
        .map_err(|err| format!("session.start: {err}"))?;
    let session_id = result
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "session.start returned no session_id".to_string())?;
    chats.update(|cs| {
        if let Some(chat) = cs.get_mut(idx) {
            chat.session = Some(session_id.clone());
            chat.backend = "session".into();
        }
    });
    Ok(session_id)
}

pub(crate) struct DelegateTurn<'a> {
    pub(crate) token: &'a str,
    pub(crate) existing: Option<String>,
    pub(crate) turn_id: &'a str,
    pub(crate) text: &'a str,
    pub(crate) agent: &'a str,
    pub(crate) autonomy: &'a str,
    pub(crate) model: &'a str,
    pub(crate) root: &'a str,
}

pub(crate) async fn send_delegate_turn(turn: DelegateTurn<'_>) -> Result<(), String> {
    let cmd = delegate_command(
        turn.existing,
        turn.text,
        turn.agent,
        turn.autonomy,
        turn.model,
        turn.root,
    );
    start_job_with_id(turn.token, turn.turn_id, cmd)
        .await
        .map_err(|err| format!("delegate turn: {err}"))
}

fn delegate_command(
    existing: Option<String>,
    text: &str,
    agent: &str,
    autonomy: &str,
    model: &str,
    root: &str,
) -> serde_json::Value {
    match existing {
        Some(session_id) => json!({
            "kind": "delegate.steer",
            "session_id": session_id,
            "message": text,
        }),
        None => {
            let mut cmd = json!({
                "kind": "delegate.start",
                "agent": agent,
                "task": text,
                "autonomy": autonomy,
            });
            if !model.is_empty() {
                cmd["model"] = json!(model);
            }
            if !root.is_empty() {
                cmd["cwd"] = json!(root);
            }
            cmd
        }
    }
}
