//! L0c toolchain/input pinning (`docs/designs/trust-substrate.md` §5 inputs, §3
//! L0c) — the host-side seam that resolves *what a run executed in* so the captured
//! [`Run`](nerve_core::provenance::Run) commits to its closure, not just the agent's
//! output. This is the impure half of L0c: it touches the filesystem (reads
//! lockfiles under the served root) and lives above the determinism boundary in
//! `nerve-workstation`. The pure digest over the resolved pin is computed by
//! [`nerve_core::runpin::hash_toolchain`] (INV-R2: hashing is pure + golden-tested).
//!
//! What ships today is the **best-effort, capture-not-gating** floor: lockfiles
//! found under the root are read and folded into a [`ToolchainPin`]; the strong
//! hermetic-environment seam ([`EnvironmentPinner`], OCI image digest) is declared
//! but deferred — `resolve_run_inputs` always passes `image_digest: None`. A missing
//! root, an unreadable lockfile, or no lockfiles at all yields an empty/partial pin
//! rather than failing the run: provenance is an audit seam, never a gate.

use nerve_core::provenance::{RunInputs, ToolchainPin};
use std::fs;
use std::path::Path;

/// Lockfile basenames probed under the served root, in deterministic order. These
/// are the resolved-dependency manifests whose content pins the run's closure; the
/// map is keyed by basename so the digest is stable regardless of discovery order.
const LOCKFILE_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "go.sum",
    "composer.lock",
    "Gemfile.lock",
];

/// A deferred seam for *strong* environment reproduction (an OCI image digest of a
/// fully-pinned hermetic environment). No implementation ships today — the
/// best-effort floor in [`resolve_run_inputs`] passes `image_digest: None`. When a
/// hermetic launcher lands, an `impl EnvironmentPinner` is constructed at the
/// composition root and its digest threaded into [`RunInputs::image_digest`] without
/// any change to this signature (additive, L0c deferred infra).
#[allow(
    dead_code,
    reason = "declared deferred-infra seam; no impl constructed today (trust-substrate §5)"
)]
pub(crate) trait EnvironmentPinner {
    /// Resolve an OCI image digest for the closure rooted at `root`, when one can be
    /// produced; `None` when no hermetic environment is pinned.
    fn pin_environment(&self, root: &Path) -> Option<String>;
}

/// Resolve the pinned closure for a run rooted at `root`: a toolchain digest over the
/// lockfiles discovered under the root, with `repo_snapshot_hash` and `image_digest`
/// left for the (deferred) snapshot/hermetic seams. Best-effort — a `None` root or no
/// lockfiles yields an empty/partial [`RunInputs`], never an error.
pub(crate) fn resolve_run_inputs(root: Option<&Path>) -> RunInputs {
    let pin = resolve_toolchain_pin(root);
    let toolchain_digest = if pin.tools.is_empty() && pin.lockfiles.is_empty() {
        // Nothing resolved — leave the digest empty so the field is skipped and an
        // unpinned run reproduces the pre-L0c content address byte-for-byte.
        String::new()
    } else {
        nerve_core::runpin::hash_toolchain(&pin)
    };
    RunInputs {
        repo_snapshot_hash: String::new(),
        toolchain_digest,
        image_digest: None,
        // A delegated agent run is contained by the best-effort `ProcessLauncher`
        // (scrubbed env, forced cwd) — the honest `Contained` tier (INV-R7). No kernel
        // closure is enforced yet, so nothing here may claim `Hermetic`. `Contained` is
        // the default, so this is omitted on the wire (additive-invariance).
        isolation_tier: nerve_core::provenance::IsolationTier::Contained,
    }
}

/// Read the lockfiles found directly under `root` into a [`ToolchainPin`]. The
/// `lockfiles` map is keyed by basename with the file's raw content as the value, so
/// [`nerve_core::runpin::hash_toolchain`] folds a deterministic, sorted digest over
/// the resolved dependency set. `tools` (tool→version) is left empty today: version
/// probing is captured-not-gating deferred infra. A `None` root, a missing file, or
/// an unreadable file is simply skipped.
pub(crate) fn resolve_toolchain_pin(root: Option<&Path>) -> ToolchainPin {
    let mut pin = ToolchainPin::default();
    let Some(root) = root else {
        return pin;
    };
    for name in LOCKFILE_NAMES {
        let path = root.join(name);
        if let Ok(content) = fs::read_to_string(&path) {
            pin.lockfiles.insert((*name).to_string(), content);
        }
    }
    pin
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn none_root_yields_empty_inputs() {
        let inputs = resolve_run_inputs(None);
        assert!(inputs.repo_snapshot_hash.is_empty());
        assert!(inputs.toolchain_digest.is_empty());
        assert!(inputs.image_digest.is_none());
        assert_eq!(resolve_toolchain_pin(None), ToolchainPin::default());
    }

    #[test]
    fn root_without_lockfiles_is_empty_and_unpinned() {
        let dir = tempdir().unwrap();
        let pin = resolve_toolchain_pin(Some(dir.path()));
        assert!(pin.lockfiles.is_empty());
        // No lockfiles -> empty digest -> an unpinned run (skipped field).
        let inputs = resolve_run_inputs(Some(dir.path()));
        assert!(inputs.toolchain_digest.is_empty());
    }

    #[test]
    fn lockfiles_are_read_and_image_digest_stays_none() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("Cargo.lock"), "name = \"a\"\n").unwrap();
        fs::write(dir.path().join("go.sum"), "mod v1.0.0 h1:abc\n").unwrap();

        let pin = resolve_toolchain_pin(Some(dir.path()));
        assert_eq!(pin.lockfiles.len(), 2);
        assert!(pin.lockfiles.contains_key("Cargo.lock"));
        assert!(pin.lockfiles.contains_key("go.sum"));
        assert!(pin.tools.is_empty());

        let inputs = resolve_run_inputs(Some(dir.path()));
        assert!(
            !inputs.toolchain_digest.is_empty(),
            "resolved lockfiles must pin a digest"
        );
        assert!(inputs.image_digest.is_none(), "image digest deferred");
    }

    #[test]
    fn pin_is_discovery_order_independent() {
        // Two roots with the same lockfiles in different on-disk creation order
        // produce the same pin (BTreeMap keys it by basename) -> same digest.
        let make = |first_cargo: bool| {
            let dir = tempdir().unwrap();
            if first_cargo {
                fs::write(dir.path().join("Cargo.lock"), "x\n").unwrap();
                fs::write(dir.path().join("go.sum"), "y\n").unwrap();
            } else {
                fs::write(dir.path().join("go.sum"), "y\n").unwrap();
                fs::write(dir.path().join("Cargo.lock"), "x\n").unwrap();
            }
            resolve_run_inputs(Some(dir.path())).toolchain_digest
        };
        assert_eq!(make(true), make(false));
    }
}
