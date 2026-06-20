use nerve_core::CancelToken;
use nerve_runtime::{RiskTier, RuntimeEvent, SessionApprovalDecision};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::EventEmitter;
use crate::policy::{self, Approver};

/// Per-session memory of remembered approval decisions: tool name → `true`
/// (allow-always) / `false` (deny-always). Owned by the `LiveSession` and shared
/// into each turn's [`ProtocolApprover`], so an `AllowAlways` / `DenyAlways`
/// decision persists across turns for the life of the session.
pub(crate) type DecisionMemory = Arc<Mutex<HashMap<String, bool>>>;

const APPROVAL_POLL: Duration = Duration::from_millis(100);

/// Upper bound on how long a single approval request blocks the turn waiting for
/// an operator decision. If no decision arrives (e.g. the frontend never surfaced
/// the prompt, or the operator walked away), the request auto-denies rather than
/// hanging the turn — and therefore the session — forever.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// The runtime-protocol approval round-trip used by both session agent turns
/// (via [`ProtocolApprover`]) and delegated-claude tool prompts (DA-5b, via
/// [`crate::delegate_session::DelegateApprover`]). Crate-visible so the
/// [`JobManager`](crate::jobs) can route a `delegate.start` job's `can_use_tool`
/// prompts through the *same* hub the `SessionManager` resolves `session.respond`
/// against — so the TUI modal and `SessionRespond` reach delegated tool approvals
/// exactly as they do agent-tool approvals.
pub(crate) struct ApprovalHub {
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
    pub(crate) fn new(emit: Arc<EventEmitter>) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            emit,
        }
    }

    /// Emit an `approval_requested` event with the tool's real risk `tier` and a
    /// human-readable `preview`, then block (up to [`APPROVAL_TIMEOUT`]) for the
    /// operator's decision. Returns the full [`SessionApprovalDecision`] so the
    /// caller can act on `.remember()`; cancellation / timeout / a dropped
    /// responder all resolve to [`SessionApprovalDecision::Deny`].
    pub(crate) fn request(
        &self,
        session_id: &str,
        tool: &str,
        arguments: &Value,
        tier: RiskTier,
        preview: String,
        cancel: &CancelToken,
    ) -> SessionApprovalDecision {
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
            tier,
            preview,
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
        decision
    }

    pub(crate) fn respond(
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

/// DA-5b: the hub also serves a delegated `claude` session's `can_use_tool`
/// permission prompts. The blocking round-trip is identical to a session agent's
/// (`approve` above), so a delegated tool ask emits the same `approval_requested`
/// event and is resolved by the same `session.respond` — the per-session decision
/// memory lives in the delegate session, mirroring [`ProtocolApprover`].
impl crate::delegate_proxy::DelegateApprover for ApprovalHub {
    fn request(
        &self,
        session_id: &str,
        tool: &str,
        args: &Value,
        tier: RiskTier,
        preview: String,
        cancel: &CancelToken,
    ) -> SessionApprovalDecision {
        ApprovalHub::request(self, session_id, tool, args, tier, preview, cancel)
    }
}

pub(crate) struct ProtocolApprover {
    session_id: String,
    hub: Arc<ApprovalHub>,
    cancel: CancelToken,
    /// Per-session remembered decisions (tool → allow-always / deny-always),
    /// shared with the owning [`LiveSession`](super::LiveSession) so an
    /// `AllowAlways` / `DenyAlways` persists across turns. A remembered `false`
    /// also subsumes the previous per-turn auto-deny-of-repeats: once a tool is
    /// deny-always it never re-prompts, so a model that keeps re-requesting it
    /// cannot wedge the turn in `Running`.
    decisions: DecisionMemory,
}

impl ProtocolApprover {
    pub(crate) fn new(
        session_id: String,
        hub: Arc<ApprovalHub>,
        cancel: CancelToken,
        decisions: DecisionMemory,
    ) -> Self {
        Self {
            session_id,
            hub,
            cancel,
            decisions,
        }
    }
}

impl Approver for ProtocolApprover {
    fn approve(&self, tool: &str, args: &Value) -> bool {
        // A remembered decision short-circuits without a fresh prompt: allow-always
        // runs, deny-always blocks. The deny-always path also prevents a model that
        // keeps re-requesting a refused tool from blocking the turn on an approval
        // the operator already answered (which would never resolve).
        if let Some(&allow) = crate::sync::lock_recover(&self.decisions).get(tool) {
            return allow;
        }
        let tier = policy::tool_tier(tool);
        let preview = policy::format_preview(tool, args);
        let decision = self
            .hub
            .request(&self.session_id, tool, args, tier, preview, &self.cancel);
        // Record a remembered allow/deny so future calls of this tool skip the
        // prompt. A one-shot Allow/Deny is not recorded (it applies to this call
        // only). The cancel guard avoids persisting a deny that is really just an
        // interrupt of the turn.
        if decision.remember() && !self.cancel.is_cancelled() {
            crate::sync::lock_recover(&self.decisions).insert(tool.to_string(), decision.allows());
        }
        decision.allows()
    }
}
