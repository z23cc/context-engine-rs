use anyhow::{Context, Result, anyhow};
use nerve_core::{RootPolicy, WorkspaceResolver};
use nerve_fs::FsWorkspaceRegistry;
use serde_json::{Value, json};
use std::{fs, path::Path, path::PathBuf};

pub(super) fn tool_response(structured: Value, text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    })
}

pub(super) fn required_string(value: &Value, key: &str) -> Result<String> {
    optional_string(value, key).ok_or_else(|| anyhow!("missing required `{key}`"))
}

pub(super) fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn string_arg(value: &Value, key: &str, default: &str) -> String {
    optional_string(value, key).unwrap_or_else(|| default.to_string())
}

pub(super) fn timeout_arg(value: &Value, key: &str, default: u64) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(default)
}

pub(super) fn resolve_workspace_write_path(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
    key: &str,
) -> Result<PathBuf> {
    let workspace = optional_string(arguments, "workspace");
    let provider = registry.resolve_workspace(workspace.as_deref())?;
    let roots = provider
        .roots()
        .iter()
        .map(|root| root.path.clone())
        .collect();
    let policy = RootPolicy::new(roots).context("invalid workspace roots")?;
    let path = optional_string(arguments, key)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing required `{key}`"))?;
    policy.resolve_for_write(&path).map_err(Into::into)
}

pub(super) fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}
