//! Cancel-aware retry with exponential backoff for transient HTTP failures.
//!
//! A provider chat call is a single blocking request that either connects and
//! starts streaming, or fails before any byte reaches the caller's sink. That
//! pre-stream failure is exactly where a retry is safe — nothing has been
//! emitted yet — so [`post_sse`](super::http::post_sse) wraps only its
//! connect-and-status phase in [`with_retry`]. The loop is small and
//! synchronous to match the crate's `ureq` + threads style, and it sleeps in
//! short slices so a [`CancelToken`] trip (e.g. Ctrl-C) is honored within a
//! slice rather than after a full backoff.

use std::thread::sleep;
use std::time::Duration;

use nerve_core::CancelToken;

use crate::error::{AgentError, AgentResult};

/// Server-suggested `Retry-After` delays are honored up to this ceiling.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(30);
/// Granularity of cancel checks while waiting out a backoff.
const SLEEP_SLICE: Duration = Duration::from_millis(50);

/// Retry budget and backoff schedule for transient provider failures.
#[derive(Clone, Copy, Debug)]
pub struct RetryPolicy {
    /// Extra attempts after the first, so total tries == `1 + max_retries`.
    pub max_retries: u32,
    /// Backoff before the first retry; doubles on each subsequent retry.
    pub base_delay: Duration,
    /// Upper bound on any single computed backoff wait.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff for a 0-based retry index, capped at `max_delay`.
    fn backoff(&self, retry_index: u32) -> Duration {
        let factor = 1u32.checked_shl(retry_index).unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

/// The classified outcome of one attempt run by [`with_retry`].
pub enum Attempt<T> {
    /// Succeeded — return the value immediately.
    Done(T),
    /// Transient failure — retry while budget remains. `retry_after` carries a
    /// server-suggested delay (e.g. parsed from a `Retry-After` header) that
    /// overrides the computed backoff for this single wait.
    Retry {
        error: AgentError,
        retry_after: Option<Duration>,
    },
    /// Permanent failure — give up without retrying.
    Fatal(AgentError),
}

/// HTTP status codes worth retrying: rate limiting (`429`), request timeout
/// (`408`), and transient upstream errors incl. Anthropic's `529 overloaded`.
/// Every other 4xx/5xx (`400`/`401`/`404`/`422`/`501`…) is treated as permanent.
#[must_use]
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Run `attempt` until it succeeds, returns a fatal error, or the retry budget
/// is exhausted, sleeping with exponential backoff (or a server-suggested
/// `retry_after`) between tries. Returns [`AgentError::Cancelled`] as soon as
/// `cancel` trips — including during a backoff wait — without another attempt.
pub fn with_retry<T>(
    policy: &RetryPolicy,
    cancel: &CancelToken,
    mut attempt: impl FnMut() -> Attempt<T>,
) -> AgentResult<T> {
    let mut last_error: Option<AgentError> = None;
    for retry_index in 0..=policy.max_retries {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        match attempt() {
            Attempt::Done(value) => return Ok(value),
            Attempt::Fatal(error) => return Err(error),
            Attempt::Retry { error, retry_after } => {
                last_error = Some(error);
                if retry_index < policy.max_retries {
                    let delay = match retry_after {
                        Some(suggested) => suggested.min(RETRY_AFTER_CAP),
                        None => policy.backoff(retry_index),
                    };
                    backoff_sleep(delay, cancel)?;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| AgentError::Http("request failed".to_string())))
}

/// Sleep for `total`, waking every [`SLEEP_SLICE`] to check `cancel` so an
/// interrupt is honored within a slice rather than after the full backoff.
fn backoff_sleep(total: Duration, cancel: &CancelToken) -> AgentResult<()> {
    let mut remaining = total;
    while remaining > Duration::ZERO {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let nap = remaining.min(SLEEP_SLICE);
        sleep(nap);
        remaining = remaining.saturating_sub(nap);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;
    use std::time::Instant;

    fn fast_policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }
    }

    fn http(msg: &str) -> AgentError {
        AgentError::Http(msg.to_string())
    }

    #[test]
    fn returns_immediately_on_success() {
        let calls = AtomicU32::new(0);
        let out: i32 = with_retry(&fast_policy(3), &CancelToken::never(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Attempt::Done(7)
        })
        .expect("ok");
        assert_eq!(out, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retries_then_succeeds() {
        let calls = AtomicU32::new(0);
        let out: i32 = with_retry(&fast_policy(5), &CancelToken::never(), || {
            let seen = calls.fetch_add(1, Ordering::SeqCst);
            if seen < 2 {
                Attempt::Retry {
                    error: http("503"),
                    retry_after: None,
                }
            } else {
                Attempt::Done(42)
            }
        })
        .expect("eventually ok");
        assert_eq!(out, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn exhausts_budget_and_returns_last_error() {
        let calls = AtomicU32::new(0);
        let err = with_retry::<i32>(&fast_policy(2), &CancelToken::never(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Attempt::Retry {
                error: http("boom"),
                retry_after: None,
            }
        })
        .expect_err("exhausted");
        assert!(matches!(err, AgentError::Http(m) if m == "boom"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn fatal_stops_immediately() {
        let calls = AtomicU32::new(0);
        let err = with_retry::<i32>(&fast_policy(5), &CancelToken::never(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Attempt::Fatal(http("401"))
        })
        .expect_err("fatal");
        assert!(matches!(err, AgentError::Http(m) if m == "401"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelled_before_first_attempt() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let calls = AtomicU32::new(0);
        let err = with_retry::<i32>(&fast_policy(3), &cancel, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Attempt::Done(1)
        })
        .expect_err("cancelled");
        assert!(matches!(err, AgentError::Cancelled));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancel_during_backoff_aborts_promptly() {
        let policy = RetryPolicy {
            max_retries: 5,
            base_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(2),
        };
        let cancel = Arc::new(CancelToken::new());
        let background = Arc::clone(&cancel);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            background.cancel();
        });
        let start = Instant::now();
        let err = with_retry::<i32>(&policy, &cancel, || Attempt::Retry {
            error: http("503"),
            retry_after: None,
        })
        .expect_err("cancelled mid-backoff");
        handle.join().expect("background thread");
        assert!(matches!(err, AgentError::Cancelled));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "should abort within a slice, took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn classifies_statuses() {
        for status in [408, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(status), "{status} should retry");
        }
        for status in [200, 400, 401, 403, 404, 422, 501] {
            assert!(!is_retryable_status(status), "{status} should not retry");
        }
    }

    #[test]
    fn backoff_is_monotonic_and_capped() {
        let policy = RetryPolicy {
            max_retries: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        };
        assert_eq!(policy.backoff(0), Duration::from_millis(500));
        assert_eq!(policy.backoff(1), Duration::from_secs(1));
        assert_eq!(policy.backoff(2), Duration::from_secs(2));
        assert_eq!(policy.backoff(3), Duration::from_secs(4));
        assert_eq!(policy.backoff(4), Duration::from_secs(8));
        assert_eq!(policy.backoff(50), Duration::from_secs(8));
    }

    #[test]
    fn retry_after_path_terminates_with_expected_count() {
        let calls = AtomicU32::new(0);
        let err = with_retry::<i32>(&fast_policy(2), &CancelToken::never(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Attempt::Retry {
                error: http("429"),
                retry_after: Some(Duration::ZERO),
            }
        })
        .expect_err("exhausted");
        assert!(matches!(err, AgentError::Http(m) if m == "429"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
