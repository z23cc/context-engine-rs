//! Snapshot-centered context engine core.
//!
//! The core is intentionally host-agnostic: callers provide catalog data through
//! a port trait, then search/read/tree operations run against immutable snapshots.

pub mod build_context;
pub mod cancel;
pub mod catalog;
pub mod changes;
pub mod codemap;
pub mod dispatch;
pub mod edit;
pub(crate) mod graph;
pub mod ledger;
pub mod list_files;
pub mod models;
pub mod navigate;
pub mod outcome;
pub(crate) mod path_match;
pub mod policy;
pub mod port;
pub mod provenance;
pub(crate) mod ranking;
pub mod read;
pub mod receipt;
pub mod receipt_gate;
pub mod recipe;
pub mod repomap;
pub mod runpin;
pub mod search;
pub mod security;
pub mod selection;
pub(crate) mod selection_auto_codemap;
pub mod selection_rebase;
pub mod snapshot;
pub mod sync;
pub mod token;
pub mod tree;
pub mod verdict;
pub mod workspace;
pub mod workspace_context;

pub use build_context::{
    BuildContextRequest, BuildContextResponse, ScoutCitation, ScoutRange, ScoutRequest,
    ScoutResponse, build_context, build_context_cancellable, scout, scout_cancellable,
};
pub use cancel::CancelToken;
pub use catalog::{HostFile, MemoryCatalogProvider};
pub use changes::{
    AffectedSymbol, ChangedFileImpact, DetectChangesRequest, DetectChangesResponse,
    detect_changes_cancellable,
};
pub use codemap::get_code_structure;

/// Parse lightweight code symbols for one source file (provider-support facade).
///
/// Wraps the kernel-internal codemap parser so the host-side `nerve-fs` provider
/// can fill its parse cache without reaching into `pub(crate)` internals. Returns
/// the same [`port::CodeSymbolsResult`] the in-memory provider produces.
pub fn parse_symbols_for_path(source: &str, rel_path: &str) -> port::CodeSymbolsResult {
    codemap::symbols_for_path(source, rel_path).map(|maybe| maybe.map(std::sync::Arc::new))
}

/// Display language name for a path's extension (provider-support facade).
///
/// Wraps the kernel-internal language detector so the host-side `nerve-fs`
/// provider can decide which files are worth a background codemap warm.
pub fn language_name_for_path(path: &str) -> Option<&'static str> {
    codemap::path_language_name(path)
}
pub use dispatch::{
    DispatchError, dispatch_error_json, dispatch_error_json_for, dispatch_error_kind,
    dispatch_error_value, handle_tool_call, handle_tool_call_cancellable, handle_tool_call_json,
    handle_tool_call_json_cancellable, handle_tool_call_json_with_resolver,
    handle_tool_call_json_with_resolver_cancellable, handle_tool_call_with_resolver,
    handle_tool_call_with_resolver_cancellable, tool_specs,
};
pub use models::*;
pub use navigate::{
    CallDirection, CallEdge, CallHierarchyRequest, CallHierarchyResponse, DefinitionResponse,
    FindReferencingSymbolsRequest, FindReferencingSymbolsResponse, ImpactAnalysisRequest,
    ImpactAnalysisResponse, ImpactSymbol, NavigateRequest, ReadSymbolBody, ReadSymbolRequest,
    ReadSymbolResponse, ReferenceLocation, ReferencesResponse, ReferencingSymbol, SymbolLocation,
    SymbolSearchMatch, SymbolSearchRequest, SymbolSearchResponse, analyze_impact,
    analyze_impact_cancellable, call_hierarchy, call_hierarchy_cancellable, find_references,
    find_references_cancellable, find_referencing_symbols, find_referencing_symbols_cancellable,
    goto_definition, goto_definition_cancellable, read_symbol, read_symbol_cancellable,
    symbol_search, symbol_search_cancellable,
};
pub use navigate::{
    PathStep, TracePathRequest, TracePathResponse, trace_path, trace_path_cancellable,
};
pub use port::CatalogProvider;
pub use provenance::{build_ledger, build_run, hash_event};
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
pub use tree::{
    FileTreeOptions, TreeMode, get_file_tree, get_file_tree_with_selection,
    get_selected_file_tree_with_selection,
};
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

/// Test-only re-exports for the relocated provider-dependent integration tests.
///
/// The provider-dependent unit tests that used to live in `nerve-core`'s in-src
/// `#[cfg(test)]` modules now live in `crates/nerve-core/tests/` — a separate
/// compilation unit, so they can link `nerve-fs`'s `FsCatalogProvider` without the
/// `dev-dependencies` back-edge compiling `nerve-core` twice ("multiple versions
/// of crate `nerve_core`"). A few of those tests reach kernel internals (the
/// shared snapshot memos and a path-scoring helper). Rather than make those
/// internals permanently public, this module — gated behind the off-by-default
/// `test-internals` feature — re-exports exactly what the relocated tests need.
/// Plain `cargo test` (feature off) skips the `#![cfg(feature = "test-internals")]`
/// test files; CI runs them with `--features nerve-core/test-internals`.
#[cfg(feature = "test-internals")]
pub mod test_internals {
    pub use crate::dispatch::{DiffOptions, apply_changes};
    pub use crate::graph::{
        DefinitionNameIndex, shared_definition_index, shared_indexed_files, shared_reference_graph,
    };
    pub use crate::repomap::{IndexedFile, ReferenceGraph, indexed_files_cancellable};
}
