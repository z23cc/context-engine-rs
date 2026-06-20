//! The job protocol types are transport-neutral and live in `nerve-proto`
//! (re-exported from the crate root). The one engine-coupled bit — building a
//! [`RuntimeJobError`] from a [`RuntimeError`] — is provided here as an extension
//! trait, since `RuntimeError` references nerve-core and cannot live in the
//! wasm-safe protocol crate.

use crate::RuntimeError;
use nerve_proto::RuntimeJobError;

/// Bridge a [`RuntimeError`] into the protocol [`RuntimeJobError`] payload.
///
/// Kept as an extension trait (rather than an inherent method) so the protocol
/// type stays free of any nerve-core / `RuntimeError` dependency. Call as
/// `RuntimeJobError::from_runtime_error(&error)` after importing this trait.
pub trait RuntimeJobErrorExt {
    /// Build a structured job error from a runtime error (its kind + message).
    fn from_runtime_error(error: &RuntimeError) -> Self;
}

impl RuntimeJobErrorExt for RuntimeJobError {
    fn from_runtime_error(error: &RuntimeError) -> Self {
        Self::new(error.kind(), error.to_string())
    }
}
