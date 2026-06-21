//! Workspace-root and remote-daemon URL resolution/persistence for the shell.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

/// Environment variable that switches the shell to an already-running remote
/// daemon, for example `http://macbook.tailnet.ts.net:4732/`.
pub const REMOTE_URL_ENV: &str = "NERVE_REMOTE_URL";

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    last_root: Option<String>,
    remote_url: Option<String>,
    #[serde(default)]
    port: Option<u16>,
}

fn config_path(app: &AppHandle) -> Option<PathBuf> {
    let dir = app.path().app_config_dir().ok()?;
    Some(dir.join("nerve-desktop.json"))
}

fn read_persisted(app: &AppHandle) -> Option<Persisted> {
    let raw = std::fs::read_to_string(config_path(app)?).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_persisted(app: &AppHandle, persisted: &Persisted) {
    let Some(path) = config_path(app) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(persisted) {
        let _ = std::fs::write(path, json);
    }
}

/// The most recently used root, if it still exists on disk.
pub fn last_root(app: &AppHandle) -> Option<PathBuf> {
    let root = PathBuf::from(read_persisted(app)?.last_root?);
    root.is_dir().then_some(root)
}

/// Persist `root` as the last-used workspace (best effort; never fails the run).
pub fn save_last_root(app: &AppHandle, root: &Path) {
    let mut persisted = read_persisted(app).unwrap_or_default();
    persisted.last_root = Some(root.display().to_string());
    write_persisted(app, &persisted);
}

/// The persisted daemon port, reused across launches so the served GUI keeps a
/// STABLE origin (`http://127.0.0.1:<port>/`) — and thus its `localStorage`
/// (theme, settings, conversation history), which is per-origin.
pub fn saved_port(app: &AppHandle) -> Option<u16> {
    read_persisted(app)?.port
}

/// Persist the daemon `port` (best effort).
pub fn save_port(app: &AppHandle, port: u16) {
    let mut persisted = read_persisted(app).unwrap_or_default();
    persisted.port = Some(port);
    write_persisted(app, &persisted);
}

/// Resolve the remote daemon URL from `NERVE_REMOTE_URL`, else persisted config.
/// Empty values mean "local mode" on desktop; mobile callers reject that later.
pub fn resolve_remote_url(app: &AppHandle) -> Result<Option<String>, String> {
    if let Ok(raw) = std::env::var(REMOTE_URL_ENV) {
        return normalize_remote_url(&raw);
    }
    let raw = read_persisted(app).and_then(|persisted| persisted.remote_url);
    match raw {
        Some(raw) => normalize_remote_url(&raw),
        None => Ok(None),
    }
}

fn normalize_remote_url(raw: &str) -> Result<Option<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(format!(
            "remote daemon URL must start with http:// or https:// (got `{trimmed}`)"
        ));
    }
    let url = if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    };
    Ok(Some(url))
}

/// Resolve the workspace root: the `NERVE_ROOT` override, else the persisted
/// choice, else a native folder picker. Returns `None` if nothing was chosen.
pub fn resolve_root(app: &AppHandle) -> Option<PathBuf> {
    if let Ok(env_root) = std::env::var("NERVE_ROOT") {
        let candidate = PathBuf::from(env_root.trim());
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    if let Some(root) = last_root(app) {
        return Some(root);
    }
    // `blocking_pick_folder` dispatches to the UI thread and blocks the caller,
    // so it must be invoked off the main thread (we are on the supervisor thread).
    let picked = app.dialog().file().blocking_pick_folder()?;
    picked.into_path().ok()
}

#[cfg(test)]
mod tests {
    use super::normalize_remote_url;

    #[test]
    fn normalizes_remote_url() {
        assert_eq!(
            normalize_remote_url(" http://desktop.tailnet.ts.net:4732 ").unwrap(),
            Some("http://desktop.tailnet.ts.net:4732/".to_string())
        );
        assert_eq!(normalize_remote_url("   ").unwrap(), None);
        assert!(normalize_remote_url("desktop.tailnet.ts.net:4732").is_err());
    }
}
