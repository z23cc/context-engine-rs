//! Clipboard helpers for GUI handoff actions.
//!
//! Copy actions try the daemon host seam first when a daemon token is available,
//! then fall back to the browser Clipboard API. This keeps copy behavior usable in
//! plain browser mode while moving desktop-shell copies below the WebView.

use std::cell::RefCell;

use crate::rpc::start_job_await;
use leptos::prelude::*;
use serde_json::json;
use wasm_bindgen_futures::JsFuture;

thread_local! {
    static HOST_CLIPBOARD_TOKEN: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) fn set_host_clipboard_token(token: Option<String>) {
    HOST_CLIPBOARD_TOKEN.with(|stored| *stored.borrow_mut() = token);
}

pub(crate) fn copy_text_with_note(
    text: String,
    note: RwSignal<String>,
    success: impl Into<String>,
) {
    let success = success.into();
    let token = host_clipboard_token();
    leptos::task::spawn_local(async move {
        let message = if let Some(tok) = token {
            host_or_browser_write_message(&tok, text, success).await
        } else {
            browser_write_message(text, success).await
        };
        note.set(message);
    });
}

async fn host_or_browser_write_message(token: &str, text: String, success: String) -> String {
    let fallback_text = text.clone();
    let command = json!({ "kind": "host.clipboard.write_text", "text": text });
    match start_job_await(token, command).await {
        Ok(_) => success,
        Err(_) => {
            browser_write_message(
                fallback_text,
                "Copied with browser clipboard fallback.".into(),
            )
            .await
        }
    }
}

async fn browser_write_message(text: String, success: String) -> String {
    let Some(clip) = web_sys::window().map(|w| w.navigator().clipboard()) else {
        return "Clipboard unavailable; select and copy manually.".into();
    };
    match JsFuture::from(clip.write_text(&text)).await {
        Ok(_) => success,
        Err(_) => "Clipboard write failed; select and copy manually.".into(),
    }
}

fn host_clipboard_token() -> Option<String> {
    HOST_CLIPBOARD_TOKEN.with(|stored| stored.borrow().clone())
}
