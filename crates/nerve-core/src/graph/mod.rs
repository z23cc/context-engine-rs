//! Shared cross-file code-intelligence index (CodeGraph T0).
//!
//! PR1 introduces the **process-global, snapshot-identity-memoized** shared
//! `Vec<IndexedFile>`: the cross-file index every navigation / `build_context`
//! call needs is built once per snapshot and reused for as long as the provider
//! serves the same cached snapshot `Arc`. See [`memo`] for the memo key and the
//! determinism guarantees. Later PRs grow this module into the full
//! `CodeGraph` (resolver, persistence) described in `docs/designs/code-graph.md`.

mod definitions;
mod derived;
mod memo;
mod snapshot_memo;

// `pub` (not `pub(crate)`) so the gated `crate::test_internals` module can
// re-export them to the relocated integration tests. The `graph` module itself is
// `pub(crate)`, so these stay crate-private in normal builds — they are reachable
// externally ONLY through the off-by-default `test-internals` feature.
pub use definitions::{DefinitionNameIndex, shared_definition_index};
pub use derived::shared_reference_graph;
pub use memo::shared_indexed_files;
