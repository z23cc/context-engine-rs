//! Snapshot-centered context engine core.
//!
//! The core is intentionally host-agnostic: callers provide catalog data through
//! a port trait, then search/read/tree operations run against immutable snapshots.

pub mod build_context;
pub mod cancel;
pub mod catalog;
pub mod codemap;
pub mod dispatch;
pub mod edit;
pub mod models;
pub mod port;
pub mod read;
pub mod repomap;
pub mod search;
pub mod security;
pub mod selection;
pub mod snapshot;
pub mod token;
pub mod tree;
pub mod workspace;
pub mod workspace_context;

pub use build_context::{
    BuildContextRequest, BuildContextResponse, build_context, build_context_cancellable,
};
pub use cancel::CancelToken;
#[cfg(not(target_arch = "wasm32"))]
pub use catalog::{FsCatalogProvider, ScanOptions};
pub use catalog::{HostFile, MemoryCatalogProvider};
pub use codemap::get_code_structure;
pub use dispatch::{
    DispatchError, dispatch_error_json, dispatch_error_kind, handle_tool_call,
    handle_tool_call_cancellable, handle_tool_call_json, handle_tool_call_json_cancellable,
    handle_tool_call_json_with_resolver, handle_tool_call_json_with_resolver_cancellable,
    handle_tool_call_with_resolver, handle_tool_call_with_resolver_cancellable, tool_specs,
};
pub use models::*;
pub use port::CatalogProvider;
pub use read::read_file;
pub use repomap::{RepoMapRequest, get_repo_map, get_repo_map_cancellable};
pub use search::{search_snapshot, search_snapshot_cancellable};
pub use security::RootPolicy;
pub use selection::{
    LineRange, ManageSelectionMode, ManageSelectionOp, ManageSelectionRequest,
    ManageSelectionResponse, Selection, SelectionMode, SelectionSliceArg, manage_selection,
};
pub use snapshot::CatalogSnapshot;
pub use token::count_tokens;
pub use tree::{FileTreeOptions, TreeMode, get_file_tree};
#[cfg(not(target_arch = "wasm32"))]
pub use workspace::{
    ManageWorkspacesOp, ManageWorkspacesRequest, ManageWorkspacesResponse, WorkspaceInfo,
};
pub use workspace::{
    ResolvedWorkspaceProvider, SingletonWorkspaceResolver, WorkspaceId, WorkspaceRegistry,
    WorkspaceResolver,
};
pub use workspace_context::{
    WorkspaceContextInclude, WorkspaceContextRequest, WorkspaceContextResponse, workspace_context,
    workspace_context_for_selection,
};

#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    pub use crate::codemap::fuzz_symbols_for_path as codemap_symbols_for_path;
    pub use crate::search::fuzz_match_content as search_match_content;
}
