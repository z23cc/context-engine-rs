use super::DispatchError;
use crate::{CatalogProvider, edit::FileChange};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};

pub(super) fn preflight_changes<P: CatalogProvider + ?Sized>(
    provider: &P,
    changes: &[FileChange],
) -> Result<(), DispatchError> {
    let mut writes = BTreeSet::new();
    let mut deletes = BTreeSet::new();
    let mut renames = BTreeMap::new();
    for change in changes {
        match change {
            FileChange::Create { path, .. } => preflight_create(provider, path, &mut writes)?,
            FileChange::Update { path, .. } => {
                ensure_readable(provider, path)?;
                let key = path_key(path)?;
                reject(deletes.contains(&key), "delete+update conflict", path)?;
                reject(!writes.insert(key), "duplicate update/create target", path)?;
            }
            FileChange::Delete { path } => {
                ensure_readable(provider, path)?;
                let key = path_key(path)?;
                reject(writes.contains(&key), "delete+update conflict", path)?;
                reject(!deletes.insert(key), "duplicate delete", path)?;
            }
            FileChange::Rename { from, to, .. } => {
                preflight_rename(provider, from, to, &mut writes, &deletes, &mut renames)?;
            }
        }
    }
    reject_rename_cycles(&renames)
}

fn preflight_create<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: &str,
    writes: &mut BTreeSet<String>,
) -> Result<(), DispatchError> {
    let key = path_key(path)?;
    reject(!writes.insert(key), "duplicate create/update target", path)?;
    provider.validate_write_path(Path::new(path))?;
    reject(
        provider.read_bytes(Path::new(path)).is_ok(),
        "destination already exists",
        path,
    )
}

fn preflight_rename<P: CatalogProvider + ?Sized>(
    provider: &P,
    from: &str,
    to: &str,
    writes: &mut BTreeSet<String>,
    deletes: &BTreeSet<String>,
    renames: &mut BTreeMap<String, String>,
) -> Result<(), DispatchError> {
    ensure_readable(provider, from)?;
    let from_key = path_key(from)?;
    let to_key = path_key(to)?;
    reject(from_key == to_key, "rename source equals destination", from)?;
    reject(writes.contains(&from_key), "source already updated", from)?;
    reject(deletes.contains(&from_key), "source already deleted", from)?;
    reject(!writes.insert(to_key.clone()), "duplicate destination", to)?;
    provider.validate_write_path(Path::new(to))?;
    reject(
        provider.read_bytes(Path::new(to)).is_ok(),
        "destination already exists",
        to,
    )?;
    reject(
        renames.insert(from_key, to_key).is_some(),
        "duplicate rename source",
        from,
    )
}

fn reject_rename_cycles(renames: &BTreeMap<String, String>) -> Result<(), DispatchError> {
    for start in renames.keys() {
        let mut seen = BTreeSet::new();
        let mut current = start.as_str();
        while let Some(next) = renames.get(current) {
            if next == start || !seen.insert(current.to_string()) {
                return preflight_error(format!("rename cycle involving {start}"));
            }
            current = next;
        }
    }
    Ok(())
}

fn ensure_readable<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: &str,
) -> Result<(), DispatchError> {
    provider
        .read_bytes(Path::new(path))
        .map(|_| ())
        .map_err(DispatchError::Core)
}

fn path_key(path: &str) -> Result<String, DispatchError> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir | Component::Prefix(_) => {
                return preflight_error(format!("path is not batch-safe: {path}"));
            }
        }
    }
    if parts.is_empty() {
        preflight_error(format!("path is empty: {path}"))
    } else {
        Ok(parts.join("/"))
    }
}

fn reject(condition: bool, label: &str, path: &str) -> Result<(), DispatchError> {
    if condition {
        preflight_error(format!("{label}: {path}"))
    } else {
        Ok(())
    }
}

fn preflight_error<T>(detail: String) -> Result<T, DispatchError> {
    Err(DispatchError::Edit(crate::edit::EditError::Parse {
        mode: "edit-preflight",
        detail,
    }))
}
