//! Relocated provider-dependent unit tests from `nerve-core`'s in-src
//! `#[cfg(test)]` modules.
//!
//! These used to live in `nerve-core`'s in-src `#[cfg(test)]` modules but moved
//! out because they drive `nerve_fs::FsCatalogProvider` (the `dev-dependencies`
//! back-edge forbids constructing it in an in-src test, which would compile
//! `nerve-core` twice — "multiple versions of crate `nerve_core`"). They reach
//! only the public crate-root API plus `nerve_fs`, so they build/run with
//! `--features test-internals` alongside the other `relocated_*` integration
//! tests. This file reaches no kernel internals, so it is ungated (runs under a
//! plain `cargo test` too).

use nerve_core::*;
use nerve_fs::{FsCatalogProvider, ScanOptions};

// ---- changes/mod.rs ----

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn maps_diff_to_the_enclosing_changed_symbol_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn alpha() {\n    let x = 1;\n}\npub fn beta() {\n    let y = 2;\n}\n",
    )
    .expect("write");
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot_arc().expect("snapshot");

    // Change line 2 — inside `alpha`'s body; `beta` (lines 4-6) is untouched.
    let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
 pub fn alpha() {
-    let x = 1;
+    let x = 42;
 }
";
    let request = DetectChangesRequest {
        diff: diff.to_string(),
    };
    let response =
        detect_changes_cancellable(&provider, &snapshot, &request, &CancelToken::never())
            .expect("detect_changes");

    assert_eq!(response.files.len(), 1, "exactly the one changed file");
    let names: Vec<&str> = response.files[0]
        .affected
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect();
    assert!(
        names.contains(&"alpha"),
        "alpha (its body line changed) must be affected; got {names:?}"
    );
    assert!(
        !names.contains(&"beta"),
        "beta (unchanged) must not be affected; got {names:?}"
    );
}
