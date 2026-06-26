//! Catalog providers.
//!
//! `MemoryCatalogProvider` is host-fed and works in wasm/browser/edge hosts. The
//! native filesystem provider (`FsCatalogProvider`) lives **out of** this kernel,
//! host-side in the `nerve-fs` crate: the determinism boundary forbids
//! wall-clock (`Instant`), `SystemTime`, and background `std::thread` use here
//! (architecture-north-star §3.1 / INV-R2). The kernel keeps only the pure,
//! host-fed in-memory provider behind the same `CatalogProvider` port.

mod memory;
pub use memory::{HostFile, MemoryCatalogProvider};

#[cfg(test)]
mod memory_tests {
    use super::*;
    use crate::edit::FileChange;
    use crate::models::NerveError;
    use crate::port::CatalogProvider;
    use std::path::Path;

    #[test]
    fn memory_provider_reads_host_fed_files_without_fs() {
        let provider =
            MemoryCatalogProvider::new(vec![HostFile::new("src/lib.rs", "pub fn alpha() {}\n")])
                .expect("provider");
        let snapshot = provider.snapshot().expect("snapshot");
        assert_eq!(snapshot.entries[0].rel_path, "src/lib.rs");
        assert_eq!(
            provider.read_bytes(Path::new("src/lib.rs")).expect("read"),
            b"pub fn alpha() {}\n"
        );
        assert_eq!(
            provider.display_path(Path::new("src/lib.rs")),
            "host/src/lib.rs"
        );
    }

    #[test]
    fn memory_provider_applies_atomic_batches() {
        let provider =
            MemoryCatalogProvider::new(vec![HostFile::new("a.txt", "alpha\n")]).expect("provider");
        provider
            .apply_file_batch(
                &[
                    FileChange::Update {
                        path: "a.txt".to_string(),
                        content: "ALPHA\n".to_string(),
                    },
                    FileChange::Create {
                        path: "b.txt".to_string(),
                        content: "beta\n".to_string(),
                    },
                ],
                true,
            )
            .expect("atomic batch");
        assert_eq!(provider.read_bytes(Path::new("a.txt")).unwrap(), b"ALPHA\n");
        assert_eq!(provider.read_bytes(Path::new("b.txt")).unwrap(), b"beta\n");
    }

    #[test]
    fn memory_provider_rejects_traversal() {
        let err = MemoryCatalogProvider::new(vec![HostFile::new("../secret", "nope")]).unwrap_err();
        assert!(matches!(err, NerveError::PathTraversal(_)));
    }
}
