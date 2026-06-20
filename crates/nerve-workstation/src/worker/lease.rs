//! Writer-node path-leases (Wave C4) — design §6 ("Path authority") + §5.
//!
//! A deterministic, engine-level lease that forbids two **writer-nodes** (Edit or
//! Full autonomy) from running CONCURRENTLY on overlapping path scope within one
//! flow. This is two things at once (design §6):
//!
//! 1. a **safety property** — two agents must never race edits on the same paths;
//! 2. the **precondition for replay fidelity under file mutation** (design §5) — if
//!    writers were concurrent, the interleaving of their file mutations would be
//!    nondeterministic, so a later node's observed snapshot generation (and thus the
//!    recorded artifacts) could differ run-to-run. Serializing writers makes the
//!    mutation sequence a deterministic function of declared order, which keeps the
//!    byte-identical REPLAY gate honest.
//!
//! ## Scope key
//!
//! A worker's only path scope is its `cwd`, confined to the workspace root by
//! `resolve_delegate_cwd` (the `..`-escape rejection). A `Step` carries no per-step
//! path, so two writer-nodes in the same flow contend on the SAME scope — the flow
//! root. The lease is therefore keyed by that root path (a single key per flow in
//! C4); the seam generalizes to finer path keys without an engine change.
//!
//! ## Determinism
//!
//! READERS (ReadOnly autonomy) never take a lease — any number run concurrently.
//! WRITERS lock the per-scope [`Mutex`] for the duration of their turn (a plain
//! scoped guard the driver holds across `worker.start`), so at most one writer
//! touches a given scope at a time; the rest wait. The engine still writes the
//! ledger in DECLARED order (in the driver's fold loop, not inside the worker
//! threads), so the recorded tape — and the replayed one — is a function of declared
//! order, never of which writer happened to acquire the lock first.

use nerve_runtime::DelegateAutonomy;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Whether an autonomy posture is a WRITER (it may mutate files), so it must hold a
/// path-lease. `ReadOnly` is a reader and never leases; `Edit`/`Full` are writers.
#[must_use]
pub(crate) fn is_writer(autonomy: DelegateAutonomy) -> bool {
    matches!(autonomy, DelegateAutonomy::Edit | DelegateAutonomy::Full)
}

/// The per-flow registry of writer-node path-leases (design §6). One [`Mutex`] per
/// path scope; a writer-node holds its scope's lock for the duration of its turn, so
/// writers on overlapping scope are SERIALIZED — never concurrent. Cheap to clone
/// (the inner map is shared); shared across the wave's worker threads.
#[derive(Clone, Default)]
pub(crate) struct PathLeases {
    /// scope-key → the lease mutex for that scope. A `Mutex<()>` whose guard, held
    /// for a writer's turn, IS the lease.
    scopes: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl PathLeases {
    /// A fresh, empty lease registry.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The scope key a worker rooted at `root` contends on. With no per-step path,
    /// every writer in a flow shares the flow-root scope (so two writer-nodes always
    /// overlap); a rootless worker uses a stable sentinel so they still serialize.
    fn scope_key(root: Option<&Path>) -> String {
        root.map_or_else(|| "<root>".to_string(), |r| r.display().to_string())
    }

    /// The lease mutex for the scope a node of `autonomy` rooted at `root` contends
    /// on, or `None` for a READER (which never leases). The caller locks the returned
    /// mutex with a plain scoped guard held across the node's turn — so writers on the
    /// same scope serialize while readers (no mutex) stay concurrent.
    #[must_use]
    pub(crate) fn lease_for(
        &self,
        autonomy: DelegateAutonomy,
        root: Option<&Path>,
    ) -> Option<Arc<Mutex<()>>> {
        if !is_writer(autonomy) {
            return None;
        }
        let key = Self::scope_key(root);
        let mut scopes = crate::sync::lock_recover(&self.scopes);
        Some(Arc::clone(scopes.entry(key).or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn readers_never_lease() {
        let leases = PathLeases::new();
        assert!(
            leases
                .lease_for(DelegateAutonomy::ReadOnly, Some(Path::new("/p")))
                .is_none()
        );
    }

    #[test]
    fn writers_on_same_scope_share_one_lease() {
        // Two writer-nodes on the same root resolve to the SAME mutex, so holding one
        // blocks the other (serialization).
        let leases = PathLeases::new();
        let root = PathBuf::from("/proj");
        let a = leases
            .lease_for(DelegateAutonomy::Edit, Some(&root))
            .expect("writer a leases");
        let b = leases
            .lease_for(DelegateAutonomy::Full, Some(&root))
            .expect("writer b leases");
        assert!(Arc::ptr_eq(&a, &b), "same scope shares one lease mutex");
        let _held = crate::sync::lock_recover(&a);
        assert!(
            b.try_lock().is_err(),
            "the other writer is blocked while held"
        );
    }

    #[test]
    fn writers_on_distinct_scopes_do_not_block() {
        let leases = PathLeases::new();
        let a = leases
            .lease_for(DelegateAutonomy::Full, Some(Path::new("/a")))
            .expect("writer a");
        let b = leases
            .lease_for(DelegateAutonomy::Full, Some(Path::new("/b")))
            .expect("writer b");
        assert!(
            !Arc::ptr_eq(&a, &b),
            "distinct scopes are independent leases"
        );
        let _held = crate::sync::lock_recover(&a);
        assert!(b.try_lock().is_ok(), "distinct scope is not blocked");
    }
}
