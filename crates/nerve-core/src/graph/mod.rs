//! Shared cross-file code-intelligence index (CodeGraph T0).
//!
//! PR1 introduces the **process-global, snapshot-identity-memoized** shared
//! `Vec<IndexedFile>`: the cross-file index every navigation / `build_context`
//! call needs is built once per snapshot and reused for as long as the provider
//! serves the same cached snapshot `Arc`. See [`memo`] for the memo key and the
//! determinism guarantees. Later PRs grow this module into the full
//! `CodeGraph` (resolver, persistence) described in `docs/designs/code-graph.md`.

mod derived;
mod memo;

pub(crate) use derived::shared_reference_graph;
pub(crate) use memo::shared_indexed_files;
