//! The tool-permission approval modal + its `session.respond` round-trip
//! (delegate approvals share the session hub). Split out of `app.rs` to stay
//! under the file-size gate.

use crate::app::ApprovalReq;
use crate::rpc::start_job;
use leptos::{ev, leptos_dom::helpers::window_event_listener, prelude::*};
use serde_json::json;

#[component]
pub(crate) fn ApprovalModal(
    req: ApprovalReq,
    token: StoredValue<Option<String>>,
    approval: RwSignal<Option<ApprovalReq>>,
) -> impl IntoView {
    let decide = move |decision: &'static str| respond(token, approval, decision);
    let keydown = window_event_listener(ev::keydown, move |ev| {
        let key = ev.key();
        let chord = ev.meta_key() || ev.ctrl_key();
        match key.as_str() {
            "Escape" => {
                ev.prevent_default();
                respond(token, approval, "deny");
            }
            "Enter" if chord => {
                ev.prevent_default();
                respond(token, approval, "allow");
            }
            "Tab" if crate::dom::trap_tab_focus("approval-dialog", ev.shift_key()) => {
                ev.prevent_default();
            }
            _ => {}
        }
    });
    on_cleanup(move || keydown.remove());
    crate::dom::focus_approval_allow();

    let described_by = if req.preview.is_empty() {
        "approval-shortcuts"
    } else {
        "approval-preview approval-shortcuts"
    };

    view! {
        <div class="modal-scrim">
            <div id="approval-dialog" class="modal" role="alertdialog" aria-modal="true" aria-labelledby="approval-title" aria-describedby=described_by tabindex="-1">
                <div class="modal-head">
                    <span id="approval-title" class="modal-title">"Allow "<b>{req.tool.clone()}</b></span>
                    <span class=format!("tier {}", req.tier.to_lowercase())>{req.tier.clone()}</span>
                </div>
                {(!req.preview.is_empty()).then(|| view! { <pre id="approval-preview" class="modal-preview" aria-label=format!("Preview of requested tool call for {}", req.tool.clone())>{req.preview.clone()}</pre> })}
                <div id="approval-shortcuts" class="modal-shortcuts">
                    <span><kbd>"⌘/Ctrl↵"</kbd>" Allow"</span>
                    <span><kbd>"Esc"</kbd>" Deny"</span>
                    <span><kbd>"Tab"</kbd>" Move controls"</span>
                </div>
                <div class="modal-actions">
                    <button id="approval-allow" type="button" class="btn allow" title="Allow (Cmd/Ctrl+Enter)" aria-label="Allow this tool call" aria-keyshortcuts="Meta+Enter Control+Enter" on:click=move |_| decide("allow")>"Allow"</button>
                    <button type="button" class="btn" aria-label="Allow this tool for the current session" on:click=move |_| decide("allow_always")>"Allow for session"</button>
                    <button type="button" class="btn" title="Deny (Esc)" aria-label="Deny this tool call" aria-keyshortcuts="Escape" on:click=move |_| decide("deny")>"Deny"</button>
                    <button type="button" class="btn danger" aria-label="Always deny this tool" on:click=move |_| decide("deny_always")>"Always deny"</button>
                </div>
            </div>
        </div>
    }
}

/// Send a `session.respond` decision and clear the modal.
fn respond(
    token: StoredValue<Option<String>>,
    approval: RwSignal<Option<ApprovalReq>>,
    decision: &'static str,
) {
    let req = approval.get_untracked();
    approval.set(None);
    crate::dom::focus_message_input();
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
