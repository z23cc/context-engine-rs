//! Workspace routing abstractions for multi-project hosts.
//!
//! The registry is **generic over the `CatalogProvider`** and defaults to the
//! kernel-resident `MemoryCatalogProvider`. The filesystem-backed registry (with
//! `add_workspace` / `manage_workspaces` that build `FsCatalogProvider`s from
//! roots) lives host-side as `nerve_fs::FsWorkspaceRegistry` — a local newtype, so
//! its `WorkspaceResolver` impl is legal there despite the orphan rule.

use crate::{CatalogProvider, NerveError};
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
use std::{collections::HashMap, ops::Deref, sync::Arc};
#[cfg(not(target_arch = "wasm32"))]
use std::{
    path::PathBuf,
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Stable workspace identifier supplied by hosts and tool-call arguments.
pub type WorkspaceId = String;

/// Provider handle returned by workspace resolution.
pub enum ResolvedWorkspaceProvider<'a, P>
where
    P: CatalogProvider + Sync,
{
    Borrowed(&'a P),
    Shared(Arc<P>),
}

impl<P> Deref for ResolvedWorkspaceProvider<'_, P>
where
    P: CatalogProvider + Sync,
{
    type Target = P;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(provider) => provider,
            Self::Shared(provider) => provider.as_ref(),
        }
    }
}

/// Resolves an optional workspace id to the catalog provider that should handle a tool call.
pub trait WorkspaceResolver {
    type Provider: CatalogProvider + Sync;

    fn resolve_workspace(
        &self,
        workspace: Option<&str>,
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, NerveError>;

    #[cfg(not(target_arch = "wasm32"))]
    fn manage_workspaces(
        &self,
        request: ManageWorkspacesRequest,
    ) -> Result<ManageWorkspacesResponse, NerveError> {
        let _ = request;
        Err(NerveError::ManageWorkspacesUnsupported)
    }
}

/// Resolver wrapper for the legacy single-provider embedding mode.
pub struct SingletonWorkspaceResolver<'a, P>
where
    P: CatalogProvider + Sync,
{
    provider: &'a P,
}

impl<'a, P> SingletonWorkspaceResolver<'a, P>
where
    P: CatalogProvider + Sync,
{
    #[must_use]
    pub fn new(provider: &'a P) -> Self {
        Self { provider }
    }
}

impl<P> WorkspaceResolver for SingletonWorkspaceResolver<'_, P>
where
    P: CatalogProvider + Sync,
{
    type Provider = P;

    fn resolve_workspace(
        &self,
        _workspace: Option<&str>,
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, NerveError> {
        Ok(ResolvedWorkspaceProvider::Borrowed(self.provider))
    }
}

#[cfg(not(target_arch = "wasm32"))]
type WorkspaceStore<P> = RwLock<HashMap<WorkspaceId, Arc<P>>>;
#[cfg(target_arch = "wasm32")]
type WorkspaceStore<P> = RefCell<HashMap<WorkspaceId, Arc<P>>>;

/// Registry of independent catalog-provider-backed workspaces.
///
/// Generic over the provider; defaults to the kernel `MemoryCatalogProvider`. The
/// filesystem variant lives in `nerve-fs` as `FsWorkspaceRegistry`.
#[derive(Debug)]
pub struct WorkspaceRegistry<P = crate::MemoryCatalogProvider>
where
    P: CatalogProvider + Sync,
{
    workspaces: WorkspaceStore<P>,
}

impl<P> Default for WorkspaceRegistry<P>
where
    P: CatalogProvider + Sync,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<P> WorkspaceRegistry<P>
where
    P: CatalogProvider + Sync,
{
    #[must_use]
    pub fn new() -> Self {
        Self {
            workspaces: new_workspace_store(),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        workspace_len(&self.workspaces)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn insert(&self, id: impl Into<WorkspaceId>, provider: Arc<P>) -> Option<Arc<P>> {
        workspace_insert(&self.workspaces, id.into(), provider)
    }

    pub fn remove(&self, id: &str) -> Option<Arc<P>> {
        workspace_remove(&self.workspaces, id)
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<P>> {
        workspace_get(&self.workspaces, id)
    }

    /// Snapshot of every `(name, provider)` pair. Used by host registries (e.g.
    /// `nerve_fs::FsWorkspaceRegistry`) to enumerate workspaces for listing without
    /// exposing the internal store.
    #[must_use]
    pub fn entries(&self) -> Vec<(WorkspaceId, Arc<P>)> {
        workspace_entries(&self.workspaces)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn new_workspace_store<P>() -> WorkspaceStore<P> {
    RwLock::new(HashMap::new())
}

#[cfg(target_arch = "wasm32")]
fn new_workspace_store<P>() -> WorkspaceStore<P> {
    RefCell::new(HashMap::new())
}

/// Acquire a read guard on the workspace store, recovering the inner guard when a
/// prior panic poisoned the lock. A single panicking thread must not take down
/// workspace resolution server-wide, so a poisoned lock is treated as recoverable
/// rather than propagated as a panic. Happy-path behaviour is unchanged.
#[cfg(not(target_arch = "wasm32"))]
fn read_store<P>(store: &WorkspaceStore<P>) -> RwLockReadGuard<'_, HashMap<WorkspaceId, Arc<P>>> {
    store.read().unwrap_or_else(PoisonError::into_inner)
}

/// Acquire a write guard on the workspace store, recovering from a poisoned lock
/// (see [`read_store`]).
#[cfg(not(target_arch = "wasm32"))]
fn write_store<P>(store: &WorkspaceStore<P>) -> RwLockWriteGuard<'_, HashMap<WorkspaceId, Arc<P>>> {
    store.write().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_len<P>(store: &WorkspaceStore<P>) -> usize {
    read_store(store).len()
}

#[cfg(target_arch = "wasm32")]
fn workspace_len<P>(store: &WorkspaceStore<P>) -> usize {
    store.borrow().len()
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_insert<P>(
    store: &WorkspaceStore<P>,
    id: WorkspaceId,
    provider: Arc<P>,
) -> Option<Arc<P>> {
    write_store(store).insert(id, provider)
}

#[cfg(target_arch = "wasm32")]
fn workspace_insert<P>(
    store: &WorkspaceStore<P>,
    id: WorkspaceId,
    provider: Arc<P>,
) -> Option<Arc<P>> {
    store.borrow_mut().insert(id, provider)
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_remove<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    write_store(store).remove(id)
}

#[cfg(target_arch = "wasm32")]
fn workspace_remove<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    store.borrow_mut().remove(id)
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_get<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    read_store(store).get(id).cloned()
}

#[cfg(target_arch = "wasm32")]
fn workspace_get<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    store.borrow().get(id).cloned()
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_singleton<P>(store: &WorkspaceStore<P>) -> Option<Arc<P>> {
    read_store(store).values().next().cloned()
}

#[cfg(target_arch = "wasm32")]
fn workspace_singleton<P>(store: &WorkspaceStore<P>) -> Option<Arc<P>> {
    store.borrow().values().next().cloned()
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_entries<P>(store: &WorkspaceStore<P>) -> Vec<(WorkspaceId, Arc<P>)> {
    read_store(store)
        .iter()
        .map(|(name, provider)| (name.clone(), Arc::clone(provider)))
        .collect()
}

#[cfg(target_arch = "wasm32")]
fn workspace_entries<P>(store: &WorkspaceStore<P>) -> Vec<(WorkspaceId, Arc<P>)> {
    store
        .borrow()
        .iter()
        .map(|(name, provider)| (name.clone(), Arc::clone(provider)))
        .collect()
}

fn resolve_from_store<'a, P>(
    store: &'a WorkspaceStore<P>,
    workspace: Option<&str>,
) -> Result<ResolvedWorkspaceProvider<'a, P>, NerveError>
where
    P: CatalogProvider + Sync,
{
    // A blank workspace arg counts as "unspecified": models often fill the
    // optional `workspace` field with an empty string, which should behave like
    // omitting it (resolve to the sole workspace, or stay ambiguous when several
    // exist). A *non-empty* name must still match a registered workspace, so a
    // removed/unknown name keeps erroring rather than silently re-routing.
    let name = workspace
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty());
    let resolved = match name {
        Some(name) => workspace_get(store, name)
            .ok_or_else(|| NerveError::UnknownWorkspace(name.to_string()))?,
        None if workspace_len(store) == 1 => {
            workspace_singleton(store).ok_or(NerveError::AmbiguousWorkspace)?
        }
        None => return Err(NerveError::AmbiguousWorkspace),
    };
    Ok(ResolvedWorkspaceProvider::Shared(resolved))
}

/// Blanket resolver: any provider-backed registry resolves a workspace id from its
/// store. The filesystem variant (`nerve_fs::FsWorkspaceRegistry`) is a newtype
/// that overrides `manage_workspaces`; the generic registry uses the trait default
/// (`ManageWorkspacesUnsupported`).
impl<P> WorkspaceResolver for WorkspaceRegistry<P>
where
    P: CatalogProvider + Sync,
{
    type Provider = P;

    fn resolve_workspace(
        &self,
        workspace: Option<&str>,
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, NerveError> {
        resolve_from_store(&self.workspaces, workspace)
    }
}

/// One registered workspace for manage_workspaces responses.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct WorkspaceInfo {
    pub name: WorkspaceId,
    pub roots: Vec<PathBuf>,
}

/// Request for the native manage_workspaces tool.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ManageWorkspacesRequest {
    pub op: ManageWorkspacesOp,
    pub name: Option<WorkspaceId>,
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

/// Operation for manage_workspaces.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManageWorkspacesOp {
    List,
    Add,
    Remove,
    Get,
}

/// Response for the native manage_workspaces tool.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ManageWorkspacesResponse {
    pub workspaces: Vec<WorkspaceInfo>,
    pub changed: Option<WorkspaceId>,
}
