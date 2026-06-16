//! Ports that decouple the engine from data sources.

use crate::{
    cancel::CancelToken,
    codemap::{ParsedCodeFile, symbols_for_path},
    models::CtxError,
    selection::Selection,
    snapshot::CatalogSnapshot,
};
#[cfg(not(target_arch = "wasm32"))]
use std::time::SystemTime;
use std::{path::Path, sync::Arc};

/// Cached or freshly parsed lightweight code symbols for one source file.
pub type CodeSymbolsResult = Result<Option<Arc<ParsedCodeFile>>, String>;

/// Filesystem freshness signal used by native provider caches.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSignature {
    pub modified: Option<SystemTime>,
    pub size: u64,
}

/// Provider for immutable catalog snapshots and file bytes.
///
/// This is the Rust counterpart of the design document's
/// `WorkspaceSearchCatalogProviding` seam.
pub trait CatalogProvider {
    fn snapshot(&self) -> Result<CatalogSnapshot, CtxError>;

    fn snapshot_arc(&self) -> Result<Arc<CatalogSnapshot>, CtxError> {
        self.snapshot_arc_cancellable(&CancelToken::never())
    }

    fn snapshot_arc_cancellable(
        &self,
        cancel: &CancelToken,
    ) -> Result<Arc<CatalogSnapshot>, CtxError> {
        cancel.check_cancelled()?;
        let snapshot = self.snapshot().map(Arc::new)?;
        cancel.check_cancelled()?;
        Ok(snapshot)
    }

    fn invalidate(&self) {}

    fn selection(&self) -> Selection {
        Selection::default()
    }

    fn set_selection(&self, selection: Selection) {
        let _ = selection;
    }

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CtxError>;

    /// Return a native filesystem freshness signature when the provider has one.
    #[cfg(not(target_arch = "wasm32"))]
    fn file_signature(&self, _path: &Path) -> Result<Option<FileSignature>, CtxError> {
        Ok(None)
    }

    /// Write `content` to `path`, creating or overwriting it. Default: unsupported.
    fn write_text(&self, _path: &Path, _content: &str) -> Result<(), CtxError> {
        Err(CtxError::WritesUnsupported)
    }

    /// Delete the file at `path`. Default: unsupported.
    fn delete_file(&self, _path: &Path) -> Result<(), CtxError> {
        Err(CtxError::WritesUnsupported)
    }

    /// Move/rename `from` to `to`. Default: unsupported.
    fn rename_file(&self, _from: &Path, _to: &Path) -> Result<(), CtxError> {
        Err(CtxError::WritesUnsupported)
    }

    fn code_symbols_for_path(
        &self,
        path: &Path,
        rel_path: &str,
    ) -> Result<CodeSymbolsResult, CtxError> {
        let bytes = self.read_bytes(path)?;
        let source = String::from_utf8_lossy(&bytes);
        Ok(symbols_for_path(&source, rel_path).map(|maybe| maybe.map(Arc::new)))
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    fn semantic_index(&self) -> Option<Arc<crate::semantic::SemanticIndex>> {
        None
    }

    fn display_path(&self, path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }
}
