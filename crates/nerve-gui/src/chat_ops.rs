//! Small chat-state mutations shared by the root app and overlay actions.

use crate::app::Chat;
use leptos::prelude::*;

pub(crate) fn add_chat(
    chats: RwSignal<Vec<Chat>>,
    active: RwSignal<usize>,
    input: RwSignal<String>,
    error: RwSignal<Option<String>>,
    search: RwSignal<String>,
    backend: String,
) {
    let mut idx = 0;
    chats.update(|cs| {
        cs.push(Chat::new_with_backend(backend));
        idx = cs.len() - 1;
    });
    active.set(idx);
    input.set(String::new());
    error.set(None);
    search.set(String::new());
}

pub(crate) fn reset_chat(
    chats: RwSignal<Vec<Chat>>,
    idx: usize,
    input: RwSignal<String>,
    error: RwSignal<Option<String>>,
    backend: String,
) {
    chats.update(|cs| {
        if let Some(c) = cs.get_mut(idx) {
            *c = Chat::new_with_backend(backend);
        }
    });
    input.set(String::new());
    error.set(None);
}

/// Drop a chat's optimistic session id (rollback after a failed `delegate.start`).
pub(crate) fn clear_session(chats: RwSignal<Vec<Chat>>, idx: usize) {
    chats.update(|cs| {
        if let Some(c) = cs.get_mut(idx) {
            c.session = None;
        }
    });
}

/// Mark the chat's in-flight turn failed and surface the error.
pub(crate) fn fail_chat(
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
