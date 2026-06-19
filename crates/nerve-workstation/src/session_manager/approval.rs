use nerve_core::CancelToken;
use nerve_runtime::{RuntimeEvent, SessionApprovalDecision};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::EventEmitter;
use crate::policy::Approver;

const APPROVAL_POLL: Duration = Duration::from_millis(100);

/// Upper bound on how long a single approval request blocks the turn waiting for
/// an operator decision. If no decision arrives (e.g. the frontend never surfaced
/// the prompt, or the operator walked away), the request auto-denies rather than
/// hanging the turn — and therefore the session — forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

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
        crate::sync::lock_recover(&self.pending).insert(key, sender);
        (self.emit)(RuntimeEvent::approval_requested(
            session_id.to_string(),
            request_id.clone(),
            tool.to_string(),
            arguments.clone(),
        ));
        let deadline = Instant::now() + APPROVAL_TIMEOUT;
        let decision = loop {
            if cancel.is_cancelled() || Instant::now() >= deadline {
                break SessionApprovalDecision::Deny;
            }
            match receiver.recv_timeout(APPROVAL_POLL) {
                Ok(decision) => break decision,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break SessionApprovalDecision::Deny,
            }
        };
        crate::sync::lock_recover(&self.pending).remove(&ApprovalKey {
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
        crate::sync::lock_recover(&self.pending)
            .remove(&key)
            .is_some_and(|sender| sender.send(decision).is_ok())
    }
}

pub(super) struct ProtocolApprover {
    session_id: String,
    hub: Arc<ApprovalHub>,
    cancel: CancelToken,
    /// Tools the operator already denied this turn. A re-request is auto-denied
    /// without prompting again — see [`Self::approve`].
    denied: Mutex<HashSet<String>>,
}

impl ProtocolApprover {
    pub(super) fn new(session_id: String, hub: Arc<ApprovalHub>, cancel: CancelToken) -> Self {
        Self {
            session_id,
            hub,
            cancel,
            denied: Mutex::new(HashSet::new()),
        }
    }
}

impl Approver for ProtocolApprover {
    fn approve(&self, tool: &str, args: &Value) -> bool {
        // A tool the operator already denied this turn is auto-denied without a
        // fresh prompt. Otherwise a model that keeps re-requesting it would block
        // the turn on a new approval the operator already answered (or the
        // frontend already dismissed), which never resolves — wedging the session
        // in `Running` so no further message can be sent.
        if crate::sync::lock_recover(&self.denied).contains(tool) {
            return false;
        }
        let allowed = self.hub.request(&self.session_id, tool, args, &self.cancel);
        if !allowed && !self.cancel.is_cancelled() {
            crate::sync::lock_recover(&self.denied).insert(tool.to_string());
        }
        allowed
    }
}
