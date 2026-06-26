//! Parallel ignore-aware filesystem walk that builds an immutable catalog
//! snapshot. Moved verbatim out of the kernel (was `nerve-core` `catalog/fs_scan`)
//! so the impure walk lives host-side.

use ignore::WalkBuilder;
use nerve_core::{
    CancelToken, CatalogSnapshot,
    models::{CatalogEntry, Diagnostic, NerveError, RootRef},
};
use std::{
    mem,
    path::{Path, PathBuf},
    sync::mpsc,
};

pub(crate) fn scan_root(root: &RootRef, cancel: &CancelToken) -> ScanRootOutput {
    let mut builder = WalkBuilder::new(&root.path);
    let filter_cancel = cancel.clone();
    builder
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(move |entry| include_walk_entry(entry, &filter_cancel));

    let context = ScanRootContext {
        root_path: root.path.clone(),
        root_id: root.id.clone(),
        cancel: cancel.clone(),
    };
    let (sender, receiver) = mpsc::channel();
    builder
        .build_parallel()
        .run(|| scan_worker(context.clone(), sender.clone()));
    drop(sender);

    let mut output = ScanRootOutput::default();
    for worker_output in receiver {
        output.entries.extend(worker_output.entries);
        output.diagnostics.extend(worker_output.diagnostics);
    }
    output
}

fn include_walk_entry(entry: &ignore::DirEntry, cancel: &CancelToken) -> bool {
    if cancel.is_cancelled() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    !matches!(name.as_ref(), ".git" | "node_modules" | ".build" | "target")
}

#[derive(Clone)]
struct ScanRootContext {
    root_path: PathBuf,
    root_id: String,
    cancel: CancelToken,
}

#[derive(Default)]
pub(crate) struct ScanRootOutput {
    pub(crate) entries: Vec<CatalogEntry>,
    pub(crate) diagnostics: Vec<Diagnostic>,
}

struct ScanWorkerState {
    context: ScanRootContext,
    entries: Vec<CatalogEntry>,
    diagnostics: Vec<Diagnostic>,
    sender: Option<mpsc::Sender<ScanRootOutput>>,
}

impl Drop for ScanWorkerState {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(ScanRootOutput {
                entries: mem::take(&mut self.entries),
                diagnostics: mem::take(&mut self.diagnostics),
            });
        }
    }
}

fn scan_worker(
    context: ScanRootContext,
    sender: mpsc::Sender<ScanRootOutput>,
) -> Box<dyn FnMut(Result<ignore::DirEntry, ignore::Error>) -> ignore::WalkState + Send> {
    let mut state = ScanWorkerState {
        context,
        entries: Vec::new(),
        diagnostics: Vec::new(),
        sender: Some(sender),
    };
    Box::new(move |dent| {
        let ScanWorkerState {
            context,
            entries,
            diagnostics,
            sender: _,
        } = &mut state;
        scan_entry(dent, context, entries, diagnostics)
    })
}

fn scan_entry(
    dent: Result<ignore::DirEntry, ignore::Error>,
    context: &ScanRootContext,
    entries: &mut Vec<CatalogEntry>,
    diagnostics: &mut Vec<Diagnostic>,
) -> ignore::WalkState {
    if context.cancel.is_cancelled() {
        return ignore::WalkState::Quit;
    }
    let dent = match dent {
        Ok(dent) => dent,
        Err(err) => {
            push_scan_diagnostic(diagnostics, None, err.to_string());
            return ignore::WalkState::Continue;
        }
    };
    let path = dent.path();
    if !path.is_file() {
        return ignore::WalkState::Continue;
    }
    let metadata = match dent.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            push_scan_diagnostic(diagnostics, Some(path.to_path_buf()), err.to_string());
            return ignore::WalkState::Continue;
        }
    };
    push_catalog_entry(entries, path, metadata.len(), context);
    ignore::WalkState::Continue
}

fn push_scan_diagnostic(diagnostics: &mut Vec<Diagnostic>, path: Option<PathBuf>, message: String) {
    diagnostics.push(Diagnostic { path, message });
}

fn push_catalog_entry(
    entries: &mut Vec<CatalogEntry>,
    path: &Path,
    size: u64,
    context: &ScanRootContext,
) {
    let rel_path = path
        .strip_prefix(&context.root_path)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    entries.push(CatalogEntry {
        root_id: context.root_id.clone(),
        rel_path,
        abs_path: path.to_path_buf(),
        size,
    });
}

pub(crate) fn finalize_snapshot(
    mut entries: Vec<CatalogEntry>,
    mut diagnostics: Vec<Diagnostic>,
    roots: &[RootRef],
    max_entries: usize,
    cancel: &CancelToken,
) -> Result<CatalogSnapshot, NerveError> {
    cancel.check_cancelled()?;
    entries.sort_by(|left, right| {
        left.rel_path
            .cmp(&right.rel_path)
            .then_with(|| left.root_id.cmp(&right.root_id))
            .then_with(|| left.abs_path.cmp(&right.abs_path))
    });
    if entries.len() > max_entries {
        let dropped = entries.len() - max_entries;
        entries.truncate(max_entries);
        diagnostics.push(Diagnostic {
            path: None,
            message: format!(
                "catalog scan truncated to {max_entries} entries; dropped {dropped} entries due to max_entries limit"
            ),
        });
    }
    diagnostics.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(CatalogSnapshot {
        generation: 1,
        roots: roots.to_vec(),
        entries,
        diagnostics,
    })
}
