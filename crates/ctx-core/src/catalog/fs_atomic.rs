use super::*;
use std::io::{Read, Write};

#[derive(Debug)]
struct FsBackup {
    target: PathBuf,
    backup: Option<PathBuf>,
}

pub(super) fn apply_atomic_batch(
    provider: &FsCatalogProvider,
    changes: &[crate::edit::FileChange],
) -> Result<(), CtxError> {
    let mut backups = Vec::new();
    let mut applied_paths = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        let targets = match change_targets(provider, change) {
            Ok(targets) => targets,
            Err(err) => return fail_atomic(err, &applied_paths, &backups),
        };
        for target in targets {
            match backup_target(&target, backups.len() + index) {
                Ok(backup) => backups.push(backup),
                Err(err) => return fail_atomic(err, &applied_paths, &backups),
            }
        }
        if let Err(err) = apply_change(provider, change) {
            return fail_atomic(err, &applied_paths, &backups);
        }
        applied_paths.extend(change_display_paths(change));
    }
    cleanup_backups(&backups);
    provider.invalidate();
    Ok(())
}

fn fail_atomic(err: CtxError, applied: &[PathBuf], backups: &[FsBackup]) -> Result<(), CtxError> {
    let rollback_failed_paths = rollback_backups(backups);
    cleanup_backups(backups);
    Err(CtxError::AtomicBatchFailed {
        detail: err.to_string(),
        applied_paths: applied.to_vec(),
        rollback_failed_paths,
    })
}

fn change_targets(
    provider: &FsCatalogProvider,
    change: &crate::edit::FileChange,
) -> Result<Vec<PathBuf>, CtxError> {
    Ok(match change {
        crate::edit::FileChange::Create { path, .. } => {
            vec![provider.policy.resolve_for_write(Path::new(path))?]
        }
        crate::edit::FileChange::Update { path, .. } | crate::edit::FileChange::Delete { path } => {
            vec![provider.policy.resolve_allowed(Path::new(path))?]
        }
        crate::edit::FileChange::Rename { from, to, .. } => vec![
            provider.policy.resolve_allowed(Path::new(from))?,
            provider.policy.resolve_for_write(Path::new(to))?,
        ],
    })
}

pub(super) fn apply_change(
    provider: &FsCatalogProvider,
    change: &crate::edit::FileChange,
) -> Result<(), CtxError> {
    match change {
        crate::edit::FileChange::Create { path, content } => {
            let target = provider.policy.resolve_for_write(Path::new(path))?;
            write_new_text(&target, content)
        }
        crate::edit::FileChange::Update { path, content } => {
            let target = provider.policy.resolve_for_write(Path::new(path))?;
            write_text(&target, content)
        }
        crate::edit::FileChange::Delete { path } => {
            let target = provider.policy.resolve_allowed(Path::new(path))?;
            fs::remove_file(&target).map_err(|err| CtxError::io(target, err))
        }
        crate::edit::FileChange::Rename { from, to, content } => {
            let source = provider.policy.resolve_allowed(Path::new(from))?;
            let destination = provider.policy.resolve_for_write(Path::new(to))?;
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|err| CtxError::io(parent, err))?;
            }
            fs::rename(&source, &destination).map_err(|err| CtxError::io(&destination, err))?;
            write_text(&destination, content)
        }
    }
}

fn write_text(target: &Path, content: &str) -> Result<(), CtxError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| CtxError::io(parent, err))?;
    }
    fs::write(target, content).map_err(|err| CtxError::io(target, err))
}

fn write_new_text(target: &Path, content: &str) -> Result<(), CtxError> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|err| CtxError::io(parent, err))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|err| CtxError::io(target, err))?;
    file.write_all(content.as_bytes())
        .map_err(|err| CtxError::io(target, err))
}

fn backup_target(target: &Path, seed: usize) -> Result<FsBackup, CtxError> {
    if !target.exists() {
        return Ok(FsBackup {
            target: target.to_path_buf(),
            backup: None,
        });
    }
    let backup = copy_to_unique_backup(target, seed)?;
    Ok(FsBackup {
        target: target.to_path_buf(),
        backup: Some(backup),
    })
}

fn copy_to_unique_backup(target: &Path, seed: usize) -> Result<PathBuf, CtxError> {
    for attempt in 0..32 {
        let backup = backup_path(target, seed, attempt);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(mut output) => {
                let mut input = fs::File::open(target).map_err(|err| CtxError::io(target, err))?;
                let mut bytes = Vec::new();
                input
                    .read_to_end(&mut bytes)
                    .map_err(|err| CtxError::io(target, err))?;
                output
                    .write_all(&bytes)
                    .map_err(|err| CtxError::io(&backup, err))?;
                return Ok(backup);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(CtxError::io(&backup, err)),
        }
    }
    Err(CtxError::io(
        target,
        std::io::Error::new(std::io::ErrorKind::AlreadyExists, "backup path collision"),
    ))
}

fn backup_path(target: &Path, seed: usize, attempt: usize) -> PathBuf {
    let name = target.file_name().unwrap_or_default().to_string_lossy();
    let backup_name = format!(".{name}.ctx-bak-{}-{seed}-{attempt}", std::process::id());
    target.with_file_name(backup_name)
}

fn rollback_backups(backups: &[FsBackup]) -> Vec<PathBuf> {
    let mut failed = Vec::new();
    for backup in backups.iter().rev() {
        let result = match &backup.backup {
            Some(saved) => fs::copy(saved, &backup.target).map(|_| ()),
            None if backup.target.exists() => fs::remove_file(&backup.target),
            None => Ok(()),
        };
        if result.is_err() {
            failed.push(backup.target.clone());
        }
    }
    failed
}

fn cleanup_backups(backups: &[FsBackup]) {
    for backup in backups {
        if let Some(saved) = &backup.backup {
            let _ = fs::remove_file(saved);
        }
    }
}

fn change_display_paths(change: &crate::edit::FileChange) -> Vec<PathBuf> {
    match change {
        crate::edit::FileChange::Create { path, .. }
        | crate::edit::FileChange::Update { path, .. }
        | crate::edit::FileChange::Delete { path } => vec![PathBuf::from(path)],
        crate::edit::FileChange::Rename { from, to, .. } => {
            vec![PathBuf::from(from), PathBuf::from(to)]
        }
    }
}
