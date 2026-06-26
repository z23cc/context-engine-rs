use crate::{
    cancel::CancelToken,
    codemap::{CodeReference, CodeSymbol},
    models::{CatalogEntry, Diagnostic, NerveError},
    port::CatalogProvider,
    snapshot::CatalogSnapshot,
};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
use std::path::PathBuf;

use super::query::query_matches;

#[cfg(test)]
pub(super) fn analyze_files<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    query: Option<&str>,
) -> Result<Vec<FileAnalysisResult>, NerveError> {
    analyze_files_cancellable(provider, snapshot, query, &CancelToken::never())
}

pub(super) fn analyze_files_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    query: Option<&str>,
    cancel: &CancelToken,
) -> Result<Vec<FileAnalysisResult>, NerveError> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        snapshot
            .entries
            .par_iter()
            .map(|entry| analyze_file(provider, snapshot, entry, query, cancel))
            .collect()
    }
    #[cfg(target_arch = "wasm32")]
    {
        snapshot
            .entries
            .iter()
            .map(|entry| analyze_file(provider, snapshot, entry, query, cancel))
            .collect()
    }
}

fn analyze_file<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    entry: &CatalogEntry,
    query: Option<&str>,
    cancel: &CancelToken,
) -> Result<FileAnalysisResult, NerveError> {
    cancel.check_cancelled()?;
    let bytes = provider.read_bytes(&entry.abs_path)?;
    cancel.check_cancelled()?;
    let source = String::from_utf8_lossy(&bytes);
    let Some(parsed) = (match provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? {
        Ok(result) => result,
        Err(message) => {
            return Ok(FileAnalysisResult::Diagnostic(Diagnostic {
                path: Some(PathBuf::from(&entry.rel_path)),
                message,
            }));
        }
    }) else {
        return Ok(FileAnalysisResult::Unsupported);
    };

    Ok(FileAnalysisResult::Indexed(IndexedFile {
        path: entry.rel_path.clone(),
        display_path: display_path(snapshot, &entry.root_id, &entry.rel_path),
        abs_path: entry.abs_path.clone(),
        language: parsed.language.clone(),
        symbols: parsed.symbols.clone(),
        references: parsed.references.clone(),
        query_match: query.is_some_and(|needle| query_matches(&entry.rel_path, &source, needle)),
    }))
}

#[derive(Debug)]
pub(super) enum FileAnalysisResult {
    Indexed(IndexedFile),
    Unsupported,
    Diagnostic(Diagnostic),
}

// `pub` (in the private `analysis` submodule) so the gated `test-internals`
// re-export can expose this type + fields to the relocated integration tests; the
// private module keeps it crate-internal in normal builds.
#[derive(Debug, Clone)]
pub struct IndexedFile {
    pub path: String,
    pub display_path: String,
    pub abs_path: PathBuf,
    pub language: String,
    pub symbols: Vec<CodeSymbol>,
    pub references: Vec<CodeReference>,
    pub query_match: bool,
}

/// Parse every supported file in the snapshot and return its indexed symbols and
/// references. Shared by `get_repo_map` and the symbol-navigation tools so files
/// are parsed once through the provider's codemap cache. Unsupported files and
/// per-file parse diagnostics are dropped (navigation is best-effort, like the
/// repo-map's own indexing).
pub fn indexed_files_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    cancel: &CancelToken,
) -> Result<Vec<IndexedFile>, NerveError> {
    let mut files: Vec<IndexedFile> = analyze_files_cancellable(provider, snapshot, None, cancel)?
        .into_iter()
        .filter_map(|analysis| match analysis {
            FileAnalysisResult::Indexed(file) => Some(file),
            _ => None,
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn display_path(snapshot: &CatalogSnapshot, root_id: &str, rel_path: &str) -> String {
    if snapshot.roots.len() <= 1 {
        return rel_path.to_string();
    }
    format!("{root_id}/{rel_path}")
}
