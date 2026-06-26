//! Host-side filesystem adapter for the Nerve engine.
//!
//! This crate holds the impure `CatalogProvider` implementation that the pure
//! determinism kernel (`nerve-core`) must not contain: the real filesystem walk,
//! atomic write batches, the snapshot/codemap caches, and everything the kernel's
//! determinism boundary forbids — wall-clock reads (`Instant`), `SystemTime`
//! freshness signatures, and the background `std::thread` codemap warmer. It
//! plugs into the kernel exclusively through the declared `CatalogProvider` and
//! `WorkspaceResolver` seams (architecture-north-star §3.1, §4, INV-R2).

mod atomic;
mod provider;
mod registry;
mod scan;

pub use provider::{FsCatalogProvider, ScanOptions};
pub use registry::FsWorkspaceRegistry;
