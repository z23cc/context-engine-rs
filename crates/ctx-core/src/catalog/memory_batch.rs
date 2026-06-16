use super::*;

pub(super) fn refresh_snapshot_from_map(
    provider: &MemoryCatalogProvider,
    files: &HashMap<PathBuf, Arc<Vec<u8>>>,
) {
    let mut entries: Vec<CatalogEntry> = files
        .iter()
        .map(|(path, content)| CatalogEntry {
            root_id: provider.root_id.clone(),
            rel_path: path_to_slash_string(path),
            abs_path: path.clone(),
            size: content.len() as u64,
        })
        .collect();
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    let generation = provider
        .state
        .snapshot
        .read()
        .expect("memory snapshot lock")
        .generation
        .saturating_add(1);
    *provider
        .state
        .snapshot
        .write()
        .expect("memory snapshot lock") = Arc::new(CatalogSnapshot {
        generation,
        roots: vec![RootRef {
            id: provider.root_id.clone(),
            path: provider.root_path.clone(),
        }],
        entries,
        diagnostics: Vec::new(),
    });
    provider.invalidate();
}

pub(super) fn apply_change(
    files: &mut HashMap<PathBuf, Arc<Vec<u8>>>,
    change: &crate::edit::FileChange,
) -> Result<(), CtxError> {
    match change {
        crate::edit::FileChange::Create { path, content } => {
            let path = normalize_host_path(Path::new(path))?;
            if files.contains_key(&path) {
                return Err(memory_io(
                    &path,
                    std::io::ErrorKind::AlreadyExists,
                    "file exists",
                ));
            }
            files.insert(path, Arc::new(content.as_bytes().to_vec()));
        }
        crate::edit::FileChange::Update { path, content } => {
            let path = normalize_host_path(Path::new(path))?;
            files.insert(path, Arc::new(content.as_bytes().to_vec()));
        }
        crate::edit::FileChange::Delete { path } => {
            let path = normalize_host_path(Path::new(path))?;
            files
                .remove(&path)
                .ok_or_else(|| memory_io(&path, std::io::ErrorKind::NotFound, "file not found"))?;
        }
        crate::edit::FileChange::Rename { from, to, content } => {
            let from = normalize_host_path(Path::new(from))?;
            let to = normalize_host_path(Path::new(to))?;
            files
                .remove(&from)
                .ok_or_else(|| memory_io(&from, std::io::ErrorKind::NotFound, "file not found"))?;
            files.insert(to, Arc::new(content.as_bytes().to_vec()));
        }
    }
    Ok(())
}

fn memory_io(path: &Path, kind: std::io::ErrorKind, message: &str) -> CtxError {
    CtxError::io(path.to_path_buf(), std::io::Error::new(kind, message))
}
