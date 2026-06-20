//! DA-5a: the live-session registry that turns the one-shot `delegate.start` JOB
//! into a persistent, steerable session.
//!
//! The DA-2 `delegate.start` job spawns a CLI, streams to completion, and the job
//! goes terminal. DA-5a keeps a `claude` [`DelegateSession`] **alive** after turn
//! 1: the `delegate.start` job thread *parks* (status stays `running`) holding the
//! live session in this registry, while separate `delegate.steer` / `delegate.close`
//! commands reach in by `session_id` (== the start job's id) to run further turns
//! or end it. The job becomes terminal only when the session is closed (explicit
//! close, job cancel, or the child exiting).
//!
//! Concurrency: each registered session is an [`Arc<LiveHandle>`]; a turn runs
//! under the handle's `session` mutex, so a steer can never overlap turn 1 or
//! another steer. The close signal is a `Condvar` the parked start thread waits on.

use crate::delegate_session::DelegateSession;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

/// One live delegated session, keyed by the originating `delegate.start` job id.
pub(crate) struct LiveHandle {
    /// The live session. `None` once closed/reaped, so a late steer sees a clear
    /// "closed" error rather than touching a dead child.
    session: Mutex<Option<DelegateSession>>,
    /// Set when close (or cancel) is requested; the parked start thread waits on
    /// `close_cv` for it to flip, then tears the session down.
    close_requested: Mutex<bool>,
    close_cv: Condvar,
}

impl LiveHandle {
    fn new(session: DelegateSession) -> Self {
        Self {
            session: Mutex::new(Some(session)),
            close_requested: Mutex::new(false),
            close_cv: Condvar::new(),
        }
    }

    /// Run one steer turn under the session lock, forwarding assistant text to
    /// `on_progress`. Returns `Err(closed)` if the session was already torn down.
    pub(crate) fn steer(
        &self,
        message: &str,
        cancel: &nerve_core::CancelToken,
        on_progress: &mut dyn FnMut(&str),
    ) -> Result<crate::delegate_session::TurnResult, LiveError> {
        let mut guard = crate::sync::lock_recover(&self.session);
        let session = guard.as_mut().ok_or(LiveError::Closed)?;
        session
            .steer(message, cancel, on_progress)
            .map_err(|err| LiveError::Session(err.to_string()))
    }

    /// Signal the parked start thread to close: flip the flag and wake it.
    pub(crate) fn request_close(&self) {
        let mut requested = crate::sync::lock_recover(&self.close_requested);
        *requested = true;
        self.close_cv.notify_all();
    }

    /// Block until close is requested (by [`Self::request_close`]). Called by the
    /// parked `delegate.start` thread after turn 1.
    fn wait_for_close(&self) {
        let mut requested = crate::sync::lock_recover(&self.close_requested);
        while !*requested {
            requested = self
                .close_cv
                .wait(requested)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Tear the live session down (close stdin / reap, or force-kill on cancel).
    /// Idempotent: a second call finds the session already taken.
    fn shutdown(&self) {
        if let Some(mut session) = crate::sync::lock_recover(&self.session).take() {
            session.close();
        }
    }
}

/// A live-session lookup/operation failure surfaced to a steer/close caller.
#[derive(Debug)]
pub(crate) enum LiveError {
    /// No live session is registered under the given id.
    Unknown(String),
    /// The session was already closed (its child reaped).
    Closed,
    /// A turn-level failure from the underlying [`DelegateSession`].
    Session(String),
}

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "no live delegated session `{id}` (it may have ended)"),
            Self::Closed => write!(f, "delegated session is already closed"),
            Self::Session(message) => write!(f, "{message}"),
        }
    }
}

/// The registry of live delegated sessions held by the [`JobManager`](crate::jobs).
#[derive(Default)]
pub(crate) struct LiveSessions {
    sessions: Mutex<HashMap<String, Arc<LiveHandle>>>,
}

impl LiveSessions {
    /// Register a freshly-started session under its start-job id, returning the
    /// shared handle the parked thread parks on.
    pub(crate) fn register(&self, session_id: &str, session: DelegateSession) -> Arc<LiveHandle> {
        let handle = Arc::new(LiveHandle::new(session));
        crate::sync::lock_recover(&self.sessions)
            .insert(session_id.to_string(), Arc::clone(&handle));
        handle
    }

    /// Look up a registered session by id (for a steer/close routed as its own
    /// command).
    pub(crate) fn get(&self, session_id: &str) -> Result<Arc<LiveHandle>, LiveError> {
        crate::sync::lock_recover(&self.sessions)
            .get(session_id)
            .cloned()
            .ok_or_else(|| LiveError::Unknown(session_id.to_string()))
    }

    /// Park the start thread until close is requested, then shut the session down
    /// and deregister it. Holding the `Arc` keeps the handle alive for steers even
    /// though it's removed from the map at the end.
    pub(crate) fn park_until_closed(&self, session_id: &str, handle: &Arc<LiveHandle>) {
        handle.wait_for_close();
        handle.shutdown();
        crate::sync::lock_recover(&self.sessions).remove(session_id);
    }

    /// Request close + deregister for an explicit `delegate.close` or a job cancel.
    /// Returns whether a session was found (so close can report unknown ids).
    pub(crate) fn close(&self, session_id: &str) -> Result<(), LiveError> {
        let handle = self.get(session_id)?;
        handle.request_close();
        Ok(())
    }
}
