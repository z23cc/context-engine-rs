//! Workspace routing abstractions for multi-project hosts.

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
use crate::semantic::SemanticRuntimeConfig;
use crate::{CatalogProvider, CtxError};
#[cfg(not(target_arch = "wasm32"))]
use crate::{FsCatalogProvider, RootPolicy, ScanOptions, models::RootRef};
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
use std::{collections::HashMap, ops::Deref, sync::Arc};
#[cfg(not(target_arch = "wasm32"))]
use std::{path::PathBuf, sync::RwLock};

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
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, CtxError>;

    #[cfg(not(target_arch = "wasm32"))]
    fn manage_workspaces(
        &self,
        request: ManageWorkspacesRequest,
    ) -> Result<ManageWorkspacesResponse, CtxError> {
        let _ = request;
        Err(CtxError::ManageWorkspacesUnsupported)
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
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, CtxError> {
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

#[cfg(not(target_arch = "wasm32"))]
fn workspace_len<P>(store: &WorkspaceStore<P>) -> usize {
    store.read().expect("workspace lock").len()
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
    store.write().expect("workspace lock").insert(id, provider)
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
    store.write().expect("workspace lock").remove(id)
}

#[cfg(target_arch = "wasm32")]
fn workspace_remove<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    store.borrow_mut().remove(id)
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_get<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    store.read().expect("workspace lock").get(id).cloned()
}

#[cfg(target_arch = "wasm32")]
fn workspace_get<P>(store: &WorkspaceStore<P>, id: &str) -> Option<Arc<P>> {
    store.borrow().get(id).cloned()
}

#[cfg(not(target_arch = "wasm32"))]
fn workspace_singleton<P>(store: &WorkspaceStore<P>) -> Option<Arc<P>> {
    store
        .read()
        .expect("workspace lock")
        .values()
        .next()
        .cloned()
}

#[cfg(target_arch = "wasm32")]
fn workspace_singleton<P>(store: &WorkspaceStore<P>) -> Option<Arc<P>> {
    store.borrow().values().next().cloned()
}

fn resolve_from_store<'a, P>(
    store: &'a WorkspaceStore<P>,
    workspace: Option<&str>,
) -> Result<ResolvedWorkspaceProvider<'a, P>, CtxError>
where
    P: CatalogProvider + Sync,
{
    let resolved = if let Some(workspace) = workspace {
        workspace_get(store, workspace)
            .ok_or_else(|| CtxError::UnknownWorkspace(workspace.to_string()))?
    } else if workspace_len(store) == 1 {
        workspace_singleton(store).ok_or(CtxError::AmbiguousWorkspace)?
    } else {
        return Err(CtxError::AmbiguousWorkspace);
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
    ) -> Result<Option<Arc<FsCatalogProvider>>, CtxError> {
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
        let mut workspaces: Vec<_> = self
            .workspaces
            .read()
            .expect("workspace lock")
            .iter()
            .map(|(name, provider)| workspace_info(name, provider.roots()))
            .collect();
        workspaces.sort_by(|left, right| left.name.cmp(&right.name));
        workspaces
    }

    fn workspace(&self, name: &str) -> Result<WorkspaceInfo, CtxError> {
        let provider = self
            .get(name)
            .ok_or_else(|| CtxError::UnknownWorkspace(name.to_string()))?;
        Ok(workspace_info(name, provider.roots()))
    }

    fn add_from_request(
        &self,
        name: WorkspaceId,
        roots: Vec<PathBuf>,
    ) -> Result<ManageWorkspacesResponse, CtxError> {
        self.add_workspace(name.clone(), roots)?;
        Ok(ManageWorkspacesResponse {
            workspaces: vec![self.workspace(&name)?],
            changed: Some(name),
        })
    }

    fn remove_from_request(&self, name: WorkspaceId) -> Result<ManageWorkspacesResponse, CtxError> {
        self.remove(&name)
            .ok_or_else(|| CtxError::UnknownWorkspace(name.clone()))?;
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
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, CtxError> {
        resolve_from_store(&self.workspaces, workspace)
    }

    fn manage_workspaces(
        &self,
        request: ManageWorkspacesRequest,
    ) -> Result<ManageWorkspacesResponse, CtxError> {
        match request.op {
            ManageWorkspacesOp::List => Ok(ManageWorkspacesResponse {
                workspaces: self.list(),
                changed: None,
            }),
            ManageWorkspacesOp::Get => {
                let name = request.name.ok_or(CtxError::MissingWorkspaceName)?;
                Ok(ManageWorkspacesResponse {
                    workspaces: vec![self.workspace(&name)?],
                    changed: None,
                })
            }
            ManageWorkspacesOp::Add => {
                let name = request.name.ok_or(CtxError::MissingWorkspaceName)?;
                self.add_from_request(name, request.roots)
            }
            ManageWorkspacesOp::Remove => {
                let name = request.name.ok_or(CtxError::MissingWorkspaceName)?;
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
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, CtxError> {
        resolve_from_store(&self.workspaces, workspace)
    }
}
