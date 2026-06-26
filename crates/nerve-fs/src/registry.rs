//! Host-side workspace registry over the filesystem provider.
//!
//! The kernel's generic `WorkspaceRegistry<P>` (default `MemoryCatalogProvider`)
//! cannot carry the filesystem-specific `add_workspace`/`manage_workspaces`
//! surface — and the orphan rule forbids
//! `impl WorkspaceResolver for WorkspaceRegistry<FsCatalogProvider>` from a
//! downstream crate (foreign trait + foreign type with a local type parameter).
//! So this is a **local newtype** around `WorkspaceRegistry<FsCatalogProvider>`:
//! a local type makes the `WorkspaceResolver` impl (with the real
//! `manage_workspaces`) legal here, while `Deref` re-exposes the generic
//! store methods unchanged.

use crate::provider::{FsCatalogProvider, ScanOptions};
use nerve_core::{
    ManageWorkspacesOp, ManageWorkspacesRequest, ManageWorkspacesResponse, NerveError,
    ResolvedWorkspaceProvider, RootPolicy, WorkspaceId, WorkspaceInfo, WorkspaceRegistry,
    WorkspaceResolver,
};
use std::{ops::Deref, path::PathBuf, sync::Arc};

/// Registry of independent filesystem-backed workspaces.
#[derive(Debug, Default)]
pub struct FsWorkspaceRegistry {
    inner: WorkspaceRegistry<FsCatalogProvider>,
    scan_options: ScanOptions,
}

impl Deref for FsWorkspaceRegistry {
    type Target = WorkspaceRegistry<FsCatalogProvider>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl FsWorkspaceRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_scan_options(scan_options: ScanOptions) -> Self {
        Self {
            inner: WorkspaceRegistry::new(),
            scan_options,
        }
    }

    pub fn add_workspace(
        &self,
        name: impl Into<WorkspaceId>,
        roots: Vec<PathBuf>,
    ) -> Result<Option<Arc<FsCatalogProvider>>, NerveError> {
        let policy = RootPolicy::new(roots)?;
        let provider = Arc::new(FsCatalogProvider::new(policy, self.scan_options.clone()));
        Ok(self.inner.insert(name, provider))
    }

    #[must_use]
    pub fn list(&self) -> Vec<WorkspaceInfo> {
        let mut workspaces: Vec<_> = self
            .inner
            .entries()
            .into_iter()
            .map(|(name, provider)| workspace_info(&name, provider.roots()))
            .collect();
        workspaces.sort_by(|left, right| left.name.cmp(&right.name));
        workspaces
    }

    fn workspace(&self, name: &str) -> Result<WorkspaceInfo, NerveError> {
        let provider = self
            .inner
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
        self.inner
            .remove(&name)
            .ok_or_else(|| NerveError::UnknownWorkspace(name.clone()))?;
        Ok(ManageWorkspacesResponse {
            workspaces: self.list(),
            changed: Some(name),
        })
    }
}

fn workspace_info(name: &str, roots: &[nerve_core::RootRef]) -> WorkspaceInfo {
    WorkspaceInfo {
        name: name.to_string(),
        roots: roots.iter().map(|root| root.path.clone()).collect(),
    }
}

impl WorkspaceResolver for FsWorkspaceRegistry {
    type Provider = FsCatalogProvider;

    fn resolve_workspace(
        &self,
        workspace: Option<&str>,
    ) -> Result<ResolvedWorkspaceProvider<'_, Self::Provider>, NerveError> {
        self.inner.resolve_workspace(workspace)
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
