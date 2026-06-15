//! Ports that decouple the engine from data sources.

use crate::{
    cancel::CancelToken,
    codemap::{CodeSymbol, symbols_for_path},
    models::CtxError,
    snapshot::CatalogSnapshot,
};
use std::{path::Path, sync::Arc};

/// Cached or freshly parsed lightweight code symbols for one source file.
pub type CodeSymbolsResult = Result<Option<(String, Arc<Vec<CodeSymbol>>)>, String>;

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

    fn read_bytes(&self, path: &Path) -> Result<Vec<u8>, CtxError>;

    fn code_symbols_for_path(
        &self,
        path: &Path,
        rel_path: &str,
    ) -> Result<CodeSymbolsResult, CtxError> {
        let bytes = self.read_bytes(path)?;
        let source = String::from_utf8_lossy(&bytes);
        Ok(symbols_for_path(&source, rel_path)
            .map(|maybe| maybe.map(|(language, symbols)| (language, Arc::new(symbols)))))
    }

    fn display_path(&self, path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }
}
