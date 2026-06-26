//! Cooperative cancellation primitives for long-running engine operations.

use crate::models::NerveError;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[cfg(any(test, feature = "test-internals"))]
use std::sync::atomic::AtomicUsize;

/// Thread-safe cooperative cancellation token.
///
/// Long-running operations periodically call `check_cancelled`; hosts can call
/// `cancel` from another thread to request early termination.
#[derive(Clone, Debug)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    #[cfg(any(test, feature = "test-internals"))]
    cancel_after_checks: Option<Arc<AtomicUsize>>,
}

impl CancelToken {
    /// Create a token that starts in the not-cancelled state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            #[cfg(any(test, feature = "test-internals"))]
            cancel_after_checks: None,
        }
    }

    /// Return a fresh token for callers that do not want cancellation.
    #[must_use]
    pub fn never() -> Self {
        Self::new()
    }

    /// Returns true after cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Request cancellation. This is safe to call from another thread.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Return `Err(NerveError::Cancelled)` once cancellation has been requested.
    /// `pub` so host-side adapters (e.g. the `nerve-fs` provider) can honour the
    /// same cooperative-cancellation contract on their own long-running scans.
    pub fn check_cancelled(&self) -> Result<(), NerveError> {
        #[cfg(any(test, feature = "test-internals"))]
        self.apply_test_hook();

        if self.is_cancelled() {
            Err(NerveError::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Construct a token that auto-cancels after `checks` cancellation checks —
    /// a deterministic injection point for cancellation tests. `pub` only under
    /// `test-internals` (and in-crate test builds) so the relocated cancellation
    /// integration tests can drive it; never present in production builds.
    #[cfg(any(test, feature = "test-internals"))]
    pub fn cancel_after_checks(checks: usize) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            cancel_after_checks: Some(Arc::new(AtomicUsize::new(checks))),
        }
    }

    #[cfg(any(test, feature = "test-internals"))]
    fn apply_test_hook(&self) {
        let Some(remaining) = &self.cancel_after_checks else {
            return;
        };

        let mut current = remaining.load(Ordering::Relaxed);
        while current > 0 {
            match remaining.compare_exchange_weak(
                current,
                current - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(previous) => {
                    if previous == 1 {
                        self.cancel();
                    }
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::never()
    }
}
