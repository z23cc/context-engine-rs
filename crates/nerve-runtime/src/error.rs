use nerve_core::dispatch::dispatch_error_kind;
use nerve_core::{DispatchError, NerveError};
use thiserror::Error;

/// Runtime error surfaced by transport adapters.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Error returned by the built-in context-engine tool dispatcher.
    #[error(transparent)]
    Core(#[from] DispatchError),
    /// Error returned while encoding or decoding runtime JSON values.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Error message returned by a capability adapter.
    #[error("{0}")]
    Adapter(String),
    /// An executor unwound with a panic that was caught at the job boundary so the
    /// job fails terminally instead of wedging in `Running`. Kept distinct from
    /// [`Adapter`](Self::Adapter) so clients can tell an internal bug from an
    /// expected adapter-level failure.
    #[error("{0}")]
    Panic(String),
}

impl RuntimeError {
    #[must_use]
    pub fn adapter(error: impl Into<String>) -> Self {
        Self::Adapter(error.into())
    }

    #[must_use]
    pub fn cancelled() -> Self {
        Self::Core(DispatchError::Core(NerveError::Cancelled))
    }

    /// Build a non-cancelled error for an executor that panicked. The job boundary
    /// catches the unwind and maps it here, so [`is_cancelled`](Self::is_cancelled)
    /// stays `false` and the job lands `Failed` (not `Cancelled` or wedged).
    #[must_use]
    pub fn panicked(message: impl Into<String>) -> Self {
        Self::Panic(message.into())
    }

    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Core(err) => dispatch_error_kind(err),
            Self::Json(_) => "json",
            Self::Adapter(_) => "adapter",
            Self::Panic(_) => "panic",
        }
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Core(DispatchError::Core(NerveError::Cancelled)))
    }
}
