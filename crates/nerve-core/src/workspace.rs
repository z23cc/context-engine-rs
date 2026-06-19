//! Workspace routing abstractions for multi-project hosts.

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
use crate::semantic::SemanticRuntimeConfig;
use crate::{CatalogProvider, NerveError};
#[cfg(not(target_arch = "wasm32"))]
use crate::{FsCatalogProvider, RootPolicy, ScanOptions, models::RootRef};
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
#[derive(Debug)]
pub struct WorkspaceRegistry<P = NativeWorkspaceProvider>
where
    P: CatalogProvider + Sync,
{
    workspaces: WorkspaceStore<P>,
    #[cfg(not(target_arch = "wasm32"))]
    scan_options: ScanOptions,
    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    semantic: SemanticRuntimeConfig,
}

#[cfg(not(target_arch = "wasm32"))]
type NativeWorkspaceProvider = FsCatalogProvider;
#[cfg(target_arch = "wasm32")]
type NativeWorkspaceProvider = crate::MemoryCatalogProvider;

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
            #[cfg(not(target_arch = "wasm32"))]
            scan_options: ScanOptions::default(),
            #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
            semantic: SemanticRuntimeConfig::disabled(),
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

#[cfg(not(target_arch = "wasm32"))]
impl WorkspaceRegistry<FsCatalogProvider> {
    #[must_use]
    pub fn with_scan_options(scan_options: ScanOptions) -> Self {
        Self {
            workspaces: new_workspace_store(),
            scan_options,
            #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
            semantic: SemanticRuntimeConfig::disabled(),
        }
    }

    #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
    #[must_use]
    pub fn with_scan_options_and_semantic(
        scan_options: ScanOptions,
        semantic: SemanticRuntimeConfig,
    ) -> Self {
        Self {
            workspaces: new_workspace_store(),
            scan_options,
            semantic,
        }
    }

    pub fn add_workspace(
        &self,
        name: impl Into<WorkspaceId>,
        roots: Vec<PathBuf>,
    ) -> Result<Option<Arc<FsCatalogProvider>>, NerveError> {
        let policy = RootPolicy::new(roots)?;
        #[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
        let provider = {
            let semantic_index = self.semantic.build_index_for_roots(policy.roots())?;
            Arc::new(FsCatalogProvider::with_semantic_index(
                policy,
                self.scan_options.clone(),
                semantic_index,
            ))
        };
        #[cfg(not(all(feature = "semantic", not(target_arch = "wasm32"))))]
        let provider = Arc::new(FsCatalogProvider::new(policy, self.scan_options.clone()));
        Ok(self.insert(name, provider))
    }

    #[must_use]
    pub fn list(&self) -> Vec<WorkspaceInfo> {
        let mut workspaces: Vec<_> = read_store(&self.workspaces)
            .iter()
            .map(|(name, provider)| workspace_info(name, provider.roots()))
            .collect();
        workspaces.sort_by(|left, right| left.name.cmp(&right.name));
        workspaces
    }

    fn workspace(&self, name: &str) -> Result<WorkspaceInfo, NerveError> {
        let provider = self
            .get(name)
            .ok_or_else(|| NerveError::UnknownWorkspace(name.to_string()))?;
        Ok(workspace_info(name, provider.roots()))
    }

    fn add_from_request(
        &self,
        name: WorkspaceId,
        roots: Vec<PathBuf>,
    ) -> Result<ManageWorkspacesResponse, NerveError> {
        self.add_workspace(name.clone(), roots)?;
        Ok(ManageWorkspacesResponse {
            workspaces: vec![self.workspace(&name)?],
            changed: Some(name),
        })
    }

    fn remove_from_request(
        &self,
        name: WorkspaceId,
    ) -> Result<ManageWorkspacesResponse, NerveError> {
        self.remove(&name)
            .ok_or_else(|| NerveError::UnknownWorkspace(name.clone()))?;
        Ok(ManageWorkspacesResponse {
            workspaces: self.list(),
            changed: Some(name),
        })
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

#[cfg(not(target_arch = "wasm32"))]
fn workspace_info(name: &str, roots: &[RootRef]) -> WorkspaceInfo {
    WorkspaceInfo {
        name: name.to_string(),
        roots: roots.iter().map(|root| root.path.clone()).collect(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl WorkspaceResolver for WorkspaceRegistry<FsCatalogProvider> {
    type Provider = FsCatalogProvider;

    fn resolve_workspace(
        &self,
        workspace: Option<&str>,
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, NerveError> {
        resolve_from_store(&self.workspaces, workspace)
    }

    fn manage_workspaces(
        &self,
        request: ManageWorkspacesRequest,
    ) -> Result<ManageWorkspacesResponse, NerveError> {
        match request.op {
            ManageWorkspacesOp::List => Ok(ManageWorkspacesResponse {
                workspaces: self.list(),
                changed: None,
            }),
            ManageWorkspacesOp::Get => {
                let name = request.name.ok_or(NerveError::MissingWorkspaceName)?;
                Ok(ManageWorkspacesResponse {
                    workspaces: vec![self.workspace(&name)?],
                    changed: None,
                })
            }
            ManageWorkspacesOp::Add => {
                let name = request.name.ok_or(NerveError::MissingWorkspaceName)?;
                self.add_from_request(name, request.roots)
            }
            ManageWorkspacesOp::Remove => {
                let name = request.name.ok_or(NerveError::MissingWorkspaceName)?;
                self.remove_from_request(name)
            }
        }
    }
}

impl WorkspaceResolver for WorkspaceRegistry<crate::MemoryCatalogProvider> {
    type Provider = crate::MemoryCatalogProvider;

    fn resolve_workspace(
        &self,
        workspace: Option<&str>,
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, NerveError> {
        resolve_from_store(&self.workspaces, workspace)
    }
}
