//! The SSE broadcast hub behind `/events`: per-session fan-out + bounded replay.
//!
//! Split out of the HTTP transport so the routing/response half and the broadcast
//! half each stay a single responsibility. The router's single notification sink
//! feeds [`SseHub::broadcast`]; every open `/events` subscriber drains only the
//! [`SseFrame`]s its session filter selects. Lives in the transport (not in
//! `nerve-runtime`) so the protocol vocabulary stays untouched.

use super::{SSE_REPLAY_CAPACITY, SSE_SUBSCRIBER_CAPACITY};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// One fan-out unit: the rendered JSON plus the routing facts the hub extracts
/// from it once (so each subscriber doesn't re-parse). `session` scopes delivery;
/// `seq` becomes the SSE `id:` and drives `Last-Event-ID` replay.
pub(super) struct SseFrame {
    pub(super) seq: u64,
    /// `Some` for a session-scoped event (delivered only to subscribers watching
    /// that session, plus unscoped subscribers); `None` for global events.
    pub(super) session: Option<String>,
    pub(super) json: Arc<str>,
}

impl SseFrame {
    /// Build a frame from a `runtime/event` notification, lifting `event_seq` and
    /// the event's `session_id` out of `params` for routing. Both are advisory: a
    /// missing/garbled field degrades to seq `0` / unscoped rather than dropping.
    pub(super) fn from_notification(value: &Value) -> Self {
        let params = value.get("params");
        // `event_seq` serializes as camelCase `eventSeq` (the notification
        // carrier's own `rename_all`); the flattened event keeps snake_case, so
        // its `session_id` field stays `session_id`.
        let seq = params
            .and_then(|p| p.get("eventSeq"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let session = params
            .and_then(|p| p.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Self {
            seq,
            session,
            json: Arc::from(value.to_string()),
        }
    }

    /// Whether this frame should reach a subscriber with the given session filter.
    /// Unfiltered subscribers (`None`) see everything; a session subscriber sees
    /// its own session-scoped frames plus all unscoped (global) frames.
    pub(super) fn visible_to(&self, filter: Option<&str>) -> bool {
        match (filter, self.session.as_deref()) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(want), Some(have)) => want == have,
        }
    }
}

/// A broadcast hub with per-session fan-out and bounded replay: the router's
/// single notification sink feeds it, and every open `/events` subscriber
/// receives only the frames its filter selects. It lives in the transport (not in
/// `nerve-runtime`) so the protocol vocabulary stays untouched.
pub(super) struct SseHub {
    inner: Mutex<HubState>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct HubState {
    subscribers: Vec<Subscriber>,
    /// Bounded ring of recent frames for `Last-Event-ID` replay, oldest first.
    replay: VecDeque<Arc<SseFrame>>,
}

struct Subscriber {
    id: u64,
    /// `None` = unfiltered (all frames); `Some(session_id)` = only that session's
    /// frames plus global ones.
    session_filter: Option<String>,
    sender: SyncSender<Arc<SseFrame>>,
}

impl SseHub {
    pub(super) fn new() -> Self {
        Self {
            inner: Mutex::new(HubState::default()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a subscriber with an optional session filter. Returns its id, the
    /// receiver its `/events` connection drains, and any buffered frames after
    /// `after_seq` that match the filter (bounded replay on reconnect).
    pub(super) fn subscribe(
        &self,
        session_filter: Option<String>,
        after_seq: Option<u64>,
    ) -> (u64, Receiver<Arc<SseFrame>>, Vec<Arc<SseFrame>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(SSE_SUBSCRIBER_CAPACITY);
        let mut state = crate::sync::lock_recover(&self.inner);
        let backlog = match after_seq {
            Some(after) => state
                .replay
                .iter()
                .filter(|frame| frame.seq > after && frame.visible_to(session_filter.as_deref()))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        state.subscribers.push(Subscriber {
            id,
            session_filter,
            sender,
        });
        (id, receiver, backlog)
    }

    pub(super) fn unsubscribe(&self, id: u64) {
        crate::sync::lock_recover(&self.inner)
            .subscribers
            .retain(|subscriber| subscriber.id != id);
    }

    /// Record a runtime notification in the bounded replay ring and fan it out to
    /// every live subscriber whose filter selects it, dropping any whose receiver
    /// has hung up.
    pub(super) fn broadcast(&self, value: Value) {
        let frame = Arc::new(SseFrame::from_notification(&value));
        let mut state = crate::sync::lock_recover(&self.inner);
        state.replay.push_back(Arc::clone(&frame));
        while state.replay.len() > SSE_REPLAY_CAPACITY {
            state.replay.pop_front();
        }
        state.subscribers.retain(|subscriber| {
            // A subscriber the frame isn't addressed to is kept untouched. For one
            // we *do* address, a non-blocking `try_send` keeps `broadcast` from
            // ever blocking on a slow reader: a full buffer (stalled `/events`
            // reader) or a dropped receiver both prune the subscriber — a slow
            // client is treated as disconnected (it can reconnect and replay via
            // `Last-Event-ID`). A long-idle subscriber is also pruned when its
            // connection closes (its `/events` thread calls `unsubscribe`).
            if frame.visible_to(subscriber.session_filter.as_deref()) {
                match subscriber.sender.try_send(Arc::clone(&frame)) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                }
            } else {
                true
            }
        });
    }

    #[cfg(test)]
    pub(super) fn subscriber_count(&self) -> usize {
        self.inner.lock().expect("sse hub lock").subscribers.len()
    }

    #[cfg(test)]
    pub(super) fn replay_len(&self) -> usize {
        self.inner.lock().expect("sse hub lock").replay.len()
    }
}
