use nerve_core::CancelToken;
use nerve_runtime::{RuntimeEvent, SessionApprovalDecision};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::EventEmitter;
use crate::policy::Approver;

const APPROVAL_POLL: Duration = Duration::from_millis(100);

pub(super) struct ApprovalHub {
    pending: Mutex<HashMap<ApprovalKey, mpsc::Sender<SessionApprovalDecision>>>,
    next_id: AtomicU64,
    emit: Arc<EventEmitter>,
}

#[derive(Hash, PartialEq, Eq)]
struct ApprovalKey {
    session_id: String,
    request_id: String,
}

impl ApprovalHub {
    pub(super) fn new(emit: Arc<EventEmitter>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            emit,
        }
    }

    pub(super) fn request(
        &self,
        session_id: &str,
        tool: &str,
        arguments: &Value,
        cancel: &CancelToken,
    ) -> bool {
        let request_id = format!("approval-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (sender, receiver) = mpsc::channel();
        let key = ApprovalKey {
            session_id: session_id.to_string(),
            request_id: request_id.clone(),
        };
        self.pending
            .lock()
            .expect("approval lock")
            .insert(key, sender);
        (self.emit)(RuntimeEvent::approval_requested(
            session_id.to_string(),
            request_id.clone(),
            tool.to_string(),
            arguments.clone(),
        ));
        let decision = loop {
            if cancel.is_cancelled() {
                break SessionApprovalDecision::Deny;
            }
            match receiver.recv_timeout(APPROVAL_POLL) {
                Ok(decision) => break decision,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break SessionApprovalDecision::Deny,
            }
        };
        self.pending
            .lock()
            .expect("approval lock")
            .remove(&ApprovalKey {
                session_id: session_id.to_string(),
                request_id,
            });
        decision == SessionApprovalDecision::Allow
    }

    pub(super) fn respond(
        &self,
        session_id: &str,
        request_id: &str,
        decision: SessionApprovalDecision,
    ) -> bool {
        let key = ApprovalKey {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
        };
        self.pending
            .lock()
            .expect("approval lock")
            .remove(&key)
            .is_some_and(|sender| sender.send(decision).is_ok())
    }
}

pub(super) struct ProtocolApprover {
    session_id: String,
    hub: Arc<ApprovalHub>,
    cancel: CancelToken,
}

impl ProtocolApprover {
    pub(super) fn new(session_id: String, hub: Arc<ApprovalHub>, cancel: CancelToken) -> Self {
        Self {
            session_id,
            hub,
            cancel,
        }
    }
}

impl Approver for ProtocolApprover {
    fn approve(&self, tool: &str, args: &Value) -> bool {
        self.hub.request(&self.session_id, tool, args, &self.cancel)
    }
}
