use anyhow::{Context, Result, bail};
use clap::Args;
use nerve_core::RootPolicy;
use nerve_fs::{FsCatalogProvider, FsWorkspaceRegistry, ScanOptions};
use std::{collections::BTreeMap, path::PathBuf, str::FromStr};

#[derive(Debug, Args, Clone)]
pub(crate) struct ServeArgs {
    /// Allowed root for the default workspace. Repeatable. `serve` fails closed when absent.
    #[arg(long = "root")]
    pub(crate) roots: Vec<PathBuf>,
    /// Additional named workspace as name=path. Repeat to add workspaces or multiple roots per name.
    #[arg(long = "workspace")]
    pub(crate) workspaces: Vec<WorkspaceArg>,
    /// Maximum catalog entries per workspace.
    #[arg(long, default_value_t = 100_000)]
    pub(crate) max_entries: usize,
    /// Path to a JSON file listing external MCP servers to expose as tools.
    #[arg(long = "mcp-config")]
    pub(crate) mcp_config: Option<PathBuf>,
    /// Path to a JSON file defining additional model providers by config
    /// (`{ providers: [{ name, wire, base_url, api_key_env }] }`).
    #[arg(long = "provider-config")]
    pub(crate) provider_config: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceArg {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

impl FromStr for WorkspaceArg {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (name, path) = value
            .split_once('=')
            .ok_or_else(|| "workspace must be name=path".to_string())?;
        if name.is_empty() {
            return Err("workspace name must not be empty".to_string());
        }
        if path.is_empty() {
            return Err("workspace path must not be empty".to_string());
        }
        Ok(Self {
            name: name.to_string(),
            path: PathBuf::from(path),
        })
    }
}

pub(crate) fn scan_options(args: &ServeArgs) -> ScanOptions {
    ScanOptions {
        max_entries: args.max_entries,
        ..ScanOptions::default()
    }
}

pub(crate) fn provider_for_roots(
    roots: Vec<PathBuf>,
    options: ScanOptions,
    _args: &ServeArgs,
) -> Result<FsCatalogProvider> {
    let policy = RootPolicy::new(roots).context("invalid root policy")?;
    Ok(FsCatalogProvider::new(policy, options))
}

pub(crate) fn registry(args: &ServeArgs) -> Result<FsWorkspaceRegistry> {
    let options = scan_options(args);
    let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::with_scan_options(options.clone());
    registry.insert(
        "default",
        std::sync::Arc::new(provider_for_roots(
            args.roots.clone(),
            options.clone(),
            args,
        )?),
    );

    let mut grouped: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for workspace in &args.workspaces {
        if workspace.name == "default" {
            bail!("--workspace default=... conflicts with --root default workspace");
        }
        grouped
            .entry(workspace.name.clone())
            .or_default()
            .push(workspace.path.clone());
    }
    for (name, roots) in grouped {
        registry.add_workspace(name, roots)?;
    }
    Ok(registry)
}

#[cfg(test)]
pub(crate) fn args_with(roots: Vec<PathBuf>, workspaces: Vec<WorkspaceArg>) -> ServeArgs {
    ServeArgs {
        roots,
        workspaces,
        max_entries: 100_000,
        mcp_config: None,
        provider_config: None,
    }
}
