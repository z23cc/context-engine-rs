use super::*;

#[test]
fn atomic_true_unsupported_provider_fails_before_mutation() {
    #[derive(Default)]
    struct BasicProvider(std::sync::RwLock<std::collections::BTreeMap<String, String>>);
    impl CatalogProvider for BasicProvider {
        fn snapshot(&self) -> Result<crate::CatalogSnapshot, NerveError> {
            Ok(crate::CatalogSnapshot {
                generation: 0,
                roots: vec![],
                entries: vec![],
                diagnostics: vec![],
            })
        }
        fn read_bytes(&self, path: &std::path::Path) -> Result<Vec<u8>, NerveError> {
            self.0
                .read()
                .unwrap()
                .get(&path.to_string_lossy().to_string())
                .map(|text| text.as_bytes().to_vec())
                .ok_or_else(|| NerveError::OutsideRoots(path.to_path_buf()))
        }
        fn write_text(&self, path: &std::path::Path, content: &str) -> Result<(), NerveError> {
            self.0
                .write()
                .unwrap()
                .insert(path.to_string_lossy().to_string(), content.to_string());
            Ok(())
        }
    }
    let provider = BasicProvider::default();
    provider
        .write_text(std::path::Path::new("a.txt"), "alpha\n")
        .unwrap();
    let changes = vec![edit::FileChange::Update {
        path: "a.txt".to_string(),
        content: "ALPHA\n".to_string(),
    }];
    let err = apply_changes(&provider, changes, DiffOptions::default(), true)
        .err()
        .expect("atomic unsupported");
    assert!(matches!(
        err,
        DispatchError::Core(NerveError::AtomicBatchUnsupported)
    ));
    assert_eq!(
        provider.read_bytes(std::path::Path::new("a.txt")).unwrap(),
        b"alpha\n"
    );
}
