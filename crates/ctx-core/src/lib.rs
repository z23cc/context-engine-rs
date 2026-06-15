//! Snapshot-centered context engine core.
//!
//! The core is intentionally host-agnostic: callers provide catalog data through
//! a port trait, then search/read/tree operations run against immutable snapshots.

pub mod cancel;
pub mod catalog;
pub mod codemap;
pub mod dispatch;
pub mod models;
pub mod port;
pub mod read;
pub mod repomap;
pub mod search;
pub mod security;
pub mod snapshot;
pub mod tree;

pub use cancel::CancelToken;
pub use catalog::{FsCatalogProvider, ScanOptions};
pub use codemap::get_code_structure;
pub use dispatch::{
    DispatchError, dispatch_error_json, dispatch_error_kind, handle_tool_call,
    handle_tool_call_cancellable, handle_tool_call_json, handle_tool_call_json_cancellable,
    tool_specs,
};
pub use models::*;
pub use port::CatalogProvider;
pub use read::read_file;
pub use repomap::{RepoMapRequest, get_repo_map, get_repo_map_cancellable};
pub use search::{search_snapshot, search_snapshot_cancellable};
pub use security::RootPolicy;
pub use snapshot::CatalogSnapshot;
pub use tree::get_file_tree;

#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    pub use crate::codemap::fuzz_symbols_for_path as codemap_symbols_for_path;
    pub use crate::repomap::fuzz_identifier_counts as repomap_identifier_counts;
    pub use crate::search::fuzz_match_content as search_match_content;
}
