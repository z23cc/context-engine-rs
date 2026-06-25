//! Host-side (`host.*` / `workspace.*`) command executor for the [`JobManager`].
//!
//! These commands are the declared runtime seam for OS/native shell capabilities —
//! the pure core runtime deliberately does not know about windows, menus,
//! pasteboards, or process launchers. This module owns the request validation +
//! command dispatch; the per-OS pickers / openers / appearance probes live in the
//! sibling [`native`] module.

mod appearance;
mod native;

use super::JobManager;
use native::{
    open_external_url, pick_folder, pick_save_file, program_available, show_notification,
    workspace_opener, write_clipboard_text,
};
use nerve_core::{CancelToken, WorkspaceResolver};
use nerve_runtime::{HostCapabilities, HostCapabilitySupport, RuntimeCommand};
use serde_json::{Value, json};
use std::fs;

const MAX_HOST_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_NOTIFICATION_TITLE_BYTES: usize = 160;
const MAX_NOTIFICATION_BODY_BYTES: usize = 2 * 1024;
const MAX_DIALOG_TITLE_BYTES: usize = 160;
const MAX_DIALOG_NAME_BYTES: usize = 160;
const MAX_URL_BYTES: usize = 4096;

impl JobManager {
    /// Execute a host-side command. These commands are the declared runtime seam
    /// for OS/native shell capabilities; the pure core runtime deliberately does
    /// not know about windows, menus, pasteboards, or process launchers.
    pub(super) fn run_host_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::HostCapabilities => host_capabilities_value(),
            RuntimeCommand::HostClipboardWriteText { text } => run_clipboard_write_text(&text),
            RuntimeCommand::HostNotificationShow { title, body } => {
                run_notification_show(&title, body.as_deref())
            }
            RuntimeCommand::HostFolderPick { title } => run_folder_pick(title.as_deref()),
            RuntimeCommand::HostFileSaveText {
                title,
                default_name,
                text,
            } => run_file_save_text(title.as_deref(), default_name.as_deref(), &text, token),
            RuntimeCommand::HostUrlOpen { url } => run_url_open(&url),
            RuntimeCommand::WorkspaceReveal { workspace } => {
                self.run_workspace_reveal(workspace.as_deref())
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a host.* or workspace.* command",
            )),
        }
    }

    /// Reveal a served workspace root in the OS file manager (`workspace.reveal`).
    /// `workspace` selects which root when more than one is registered (single-root
    /// today). Resolves the root, then spawns the platform opener (macOS `open` /
    /// Windows `explorer` / Linux `xdg-open`) — a host side-effect, kept out of the
    /// pure engine.
    fn run_workspace_reveal(
        &self,
        workspace: Option<&str>,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let root = self
            .runtime
            .resolver()
            .resolve_workspace(workspace)
            .ok()
            .and_then(|ws| ws.roots().first().map(|root| root.path.clone()))
            .ok_or_else(|| {
                nerve_runtime::RuntimeError::adapter(
                    "workspace.reveal requires a served workspace root (start the daemon with --root)",
                )
            })?;
        std::process::Command::new(workspace_opener())
            .arg(&root)
            .spawn()
            .map_err(|err| {
                nerve_runtime::RuntimeError::adapter(format!("reveal {}: {err}", root.display()))
            })?;
        Ok(json!({ "revealed": root.to_string_lossy() }))
    }
}

/// Serialize the host-shell capability surface reachable through the daemon.
fn host_capabilities_value() -> Result<Value, nerve_runtime::RuntimeError> {
    let (scheme, accent, accent_ink) = appearance::host_appearance();
    let caps = HostCapabilities::daemon_web(
        native::host_platform(),
        HostCapabilitySupport {
            clipboard_write_text: clipboard_write_text_supported(),
            os_notifications: native::notifications_supported(),
            native_file_dialogs: native::native_file_dialogs_supported(),
            external_url_open: native::external_url_open_supported(),
            system_color_scheme: scheme,
            system_accent_color: accent,
            system_accent_ink_color: accent_ink,
        },
    );
    serde_json::to_value(caps).map_err(|err| {
        nerve_runtime::RuntimeError::adapter(format!("serialize host capabilities: {err}"))
    })
}

fn run_clipboard_write_text(text: &str) -> Result<Value, nerve_runtime::RuntimeError> {
    validate_host_text("clipboard text", text)?;
    write_clipboard_text(text)?;
    Ok(json!({ "written": true, "bytes": text.len() }))
}

fn clipboard_write_text_supported() -> bool {
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        return true;
    }
    native::clipboard_write_commands()
        .iter()
        .any(|(program, _)| program_available(program))
}

fn run_folder_pick(title: Option<&str>) -> Result<Value, nerve_runtime::RuntimeError> {
    let title = dialog_title(title, "Choose a project folder", "folder picker title")?;
    let path = pick_folder(&title)?;
    if path.is_empty() {
        return Err(nerve_runtime::RuntimeError::adapter(
            "folder picker returned an empty path",
        ));
    }
    Ok(json!({ "path": path }))
}

fn run_file_save_text(
    title: Option<&str>,
    default_name: Option<&str>,
    text: &str,
    token: &CancelToken,
) -> Result<Value, nerve_runtime::RuntimeError> {
    if token.is_cancelled() {
        return Err(nerve_runtime::RuntimeError::cancelled());
    }
    validate_host_text("file text", text)?;
    let title = dialog_title(title, "Save packet", "save panel title")?;
    let default_name = dialog_default_name(default_name)?;
    let path = pick_save_file(&title, &default_name)?;
    if token.is_cancelled() {
        return Err(nerve_runtime::RuntimeError::cancelled());
    }
    fs::write(&path, text.as_bytes()).map_err(|err| {
        nerve_runtime::RuntimeError::adapter(format!("write selected file `{path}`: {err}"))
    })?;
    Ok(json!({ "path": path, "bytes": text.len() }))
}

fn validate_host_text(label: &str, text: &str) -> Result<(), nerve_runtime::RuntimeError> {
    if text.len() > MAX_HOST_TEXT_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "{label} is too large: {} bytes exceeds {MAX_HOST_TEXT_BYTES}",
            text.len()
        )));
    }
    Ok(())
}

fn dialog_title(
    title: Option<&str>,
    fallback: &'static str,
    label: &'static str,
) -> Result<String, nerve_runtime::RuntimeError> {
    let title = title.unwrap_or(fallback).trim();
    if title.len() > MAX_DIALOG_TITLE_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "{label} is too large: {} bytes exceeds {MAX_DIALOG_TITLE_BYTES}",
            title.len()
        )));
    }
    Ok(if title.is_empty() { fallback } else { title }.to_string())
}

fn dialog_default_name(default_name: Option<&str>) -> Result<String, nerve_runtime::RuntimeError> {
    let raw = default_name.unwrap_or("nerve-packet.md").trim();
    if raw.len() > MAX_DIALOG_NAME_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "save default name is too large: {} bytes exceeds {MAX_DIALOG_NAME_BYTES}",
            raw.len()
        )));
    }
    let name = raw
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("nerve-packet.md");
    Ok(if name.is_empty() {
        "nerve-packet.md"
    } else {
        name
    }
    .to_string())
}

fn run_url_open(url: &str) -> Result<Value, nerve_runtime::RuntimeError> {
    let url = validate_external_url(url)?;
    open_external_url(&url)?;
    Ok(json!({ "opened": true, "url": url }))
}

fn validate_external_url(url: &str) -> Result<String, nerve_runtime::RuntimeError> {
    let trimmed = url.trim();
    if trimmed.len() > MAX_URL_BYTES {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "url is too large: {} bytes exceeds {MAX_URL_BYTES}",
            trimmed.len()
        )));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        Ok(trimmed.to_string())
    } else {
        Err(nerve_runtime::RuntimeError::adapter(
            "host.url.open only accepts http(s) URLs",
        ))
    }
}

fn run_notification_show(
    title: &str,
    body: Option<&str>,
) -> Result<Value, nerve_runtime::RuntimeError> {
    validate_notification_text("title", title, MAX_NOTIFICATION_TITLE_BYTES)?;
    let body = body.unwrap_or_default();
    validate_notification_text("body", body, MAX_NOTIFICATION_BODY_BYTES)?;
    show_notification(title.trim(), body.trim())?;
    Ok(json!({ "shown": true }))
}

fn validate_notification_text(
    field: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), nerve_runtime::RuntimeError> {
    if field == "title" && value.trim().is_empty() {
        return Err(nerve_runtime::RuntimeError::adapter(
            "notification title cannot be empty",
        ));
    }
    if value.len() > max_bytes {
        return Err(nerve_runtime::RuntimeError::adapter(format!(
            "notification {field} is too large: {} bytes exceeds {max_bytes}",
            value.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_url_validation_accepts_only_external_http_urls() {
        assert_eq!(
            validate_external_url(" https://example.com/auth ").expect("https URL accepted"),
            "https://example.com/auth"
        );
        assert_eq!(
            validate_external_url("http://localhost:1455/auth/callback")
                .expect("http loopback URL accepted"),
            "http://localhost:1455/auth/callback"
        );

        for url in [
            "file:///tmp/secret",
            "nerve://auth/callback",
            "javascript:alert(1)",
            "mailto:security@example.com",
            "",
        ] {
            assert!(
                validate_external_url(url).is_err(),
                "non-http(s) URL `{url}` must not reach the OS opener"
            );
        }

        let oversized = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(
            validate_external_url(&oversized).is_err(),
            "oversized URL must be rejected before invoking the host opener"
        );
    }
}
