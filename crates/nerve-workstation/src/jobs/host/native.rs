//! Per-OS native shell primitives behind the `host.*` seam: clipboard writers,
//! folder/save pickers, URL openers, notifications, and the system appearance probes
//! (color scheme + accent color). Pure platform dispatch — every entry point degrades
//! to a clear "unavailable on this platform" error rather than panicking, so the host
//! command handlers in the parent module stay platform-agnostic.

use std::io::Write;
use std::process::{Command, Stdio};

const WINDOWS_FOLDER_PICKER_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms;
$dialog = New-Object System.Windows.Forms.FolderBrowserDialog;
$dialog.Description = $args[0];
$dialog.ShowNewFolderButton = $true;
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    [Console]::Out.WriteLine($dialog.SelectedPath);
    exit 0;
}
exit 2;
"#;
const WINDOWS_SAVE_FILE_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms;
$dialog = New-Object System.Windows.Forms.SaveFileDialog;
$dialog.Title = $args[0];
$dialog.FileName = $args[1];
$dialog.Filter = "Markdown files (*.md)|*.md|Text files (*.txt)|*.txt|All files (*.*)|*.*";
$dialog.OverwritePrompt = $true;
if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {
    [Console]::Out.WriteLine($dialog.FileName);
    exit 0;
}
exit 2;
"#;

pub(super) fn write_clipboard_text(text: &str) -> Result<(), nerve_runtime::RuntimeError> {
    let mut attempts = Vec::new();
    for (program, args) in clipboard_write_commands() {
        match write_clipboard_with_command(program, args, text) {
            Ok(()) => return Ok(()),
            Err(err) => attempts.push(format!("{program}: {err}")),
        }
    }
    Err(nerve_runtime::RuntimeError::adapter(format!(
        "host clipboard write unavailable ({})",
        attempts.join("; ")
    )))
}

fn write_clipboard_with_command(program: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| err.to_string())?;
    let write_result = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(text.as_bytes())
            .map_err(|err| err.to_string()),
        None => Err("stdin unavailable".to_string()),
    };
    let status = child.wait().map_err(|err| err.to_string())?;
    write_result?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

pub(super) fn program_available(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

pub(super) fn pick_folder(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_macos_folder_picker(title);
    }
    if cfg!(target_os = "windows") {
        return run_windows_folder_picker(title);
    }
    if cfg!(target_os = "linux") {
        return run_linux_folder_picker(title);
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native folder picker is unavailable on this platform",
    ))
}

pub(super) fn pick_save_file(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_macos_save_panel(title, default_name);
    }
    if cfg!(target_os = "windows") {
        return run_windows_save_panel(title, default_name);
    }
    if cfg!(target_os = "linux") {
        return run_linux_save_panel(title, default_name);
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native save panel is unavailable on this platform",
    ))
}

pub(super) fn open_external_url(url: &str) -> Result<(), nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_status_command("open", &[url]);
    }
    if cfg!(target_os = "windows") {
        let Some(program) = windows_dialog_program() else {
            return Err(nerve_runtime::RuntimeError::adapter(
                "external URL opener is unavailable on this Windows host",
            ));
        };
        return run_status_command(
            program,
            &["-NoProfile", "-Command", "Start-Process $args[0]", url],
        );
    }
    if cfg!(target_os = "linux") {
        if program_available("xdg-open") {
            return run_status_command("xdg-open", &[url]);
        }
        if program_available("gio") {
            return run_status_command("gio", &["open", url]);
        }
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "external URL opener is unavailable on this platform",
    ))
}

pub(super) fn show_notification(
    title: &str,
    body: &str,
) -> Result<(), nerve_runtime::RuntimeError> {
    if cfg!(target_os = "macos") {
        return run_macos_notification(title, body);
    }
    if cfg!(target_os = "linux") && program_available("notify-send") {
        return run_status_command("notify-send", &[title, body]);
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "host notifications are unavailable on this platform",
    ))
}

fn run_macos_notification(title: &str, body: &str) -> Result<(), nerve_runtime::RuntimeError> {
    run_status_command(
        "osascript",
        &[
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            body,
        ],
    )
}

fn run_macos_folder_picker(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    run_output_command(
        "osascript",
        &[
            "-e",
            "on run argv",
            "-e",
            "set promptText to item 1 of argv",
            "-e",
            "set pickedFolder to choose folder with prompt promptText",
            "-e",
            "return POSIX path of pickedFolder",
            "-e",
            "end run",
            title,
        ],
        "osascript returned no folder path",
        "folder selection cancelled",
    )
}

fn run_macos_save_panel(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    run_output_command(
        "osascript",
        &[
            "-e",
            "on run argv",
            "-e",
            "set promptText to item 1 of argv",
            "-e",
            "set defaultName to item 2 of argv",
            "-e",
            "set pickedFile to choose file name with prompt promptText default name defaultName",
            "-e",
            "return POSIX path of pickedFile",
            "-e",
            "end run",
            title,
            default_name,
        ],
        "osascript returned no save path",
        "file save cancelled",
    )
}

fn run_linux_folder_picker(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    if program_available("zenity") {
        return run_output_command(
            "zenity",
            &["--file-selection", "--directory", "--title", title],
            "zenity returned no folder path",
            "folder selection cancelled",
        );
    }
    if program_available("kdialog") {
        return run_output_command(
            "kdialog",
            &["--title", title, "--getexistingdirectory"],
            "kdialog returned no folder path",
            "folder selection cancelled",
        );
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native folder picker is unavailable on this Linux host",
    ))
}

fn run_linux_save_panel(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    if program_available("zenity") {
        return run_output_command(
            "zenity",
            &[
                "--file-selection",
                "--save",
                "--confirm-overwrite",
                "--title",
                title,
                "--filename",
                default_name,
            ],
            "zenity returned no save path",
            "file save cancelled",
        );
    }
    if program_available("kdialog") {
        return run_output_command(
            "kdialog",
            &["--title", title, "--getsavefilename", default_name],
            "kdialog returned no save path",
            "file save cancelled",
        );
    }
    Err(nerve_runtime::RuntimeError::adapter(
        "native save panel is unavailable on this Linux host",
    ))
}

fn run_windows_folder_picker(title: &str) -> Result<String, nerve_runtime::RuntimeError> {
    let Some(program) = windows_dialog_program() else {
        return Err(nerve_runtime::RuntimeError::adapter(
            "native folder picker is unavailable on this Windows host",
        ));
    };
    run_output_command(
        program,
        &[
            "-NoProfile",
            "-STA",
            "-Command",
            WINDOWS_FOLDER_PICKER_SCRIPT,
            title,
        ],
        "PowerShell returned no folder path",
        "folder selection cancelled",
    )
}

fn run_windows_save_panel(
    title: &str,
    default_name: &str,
) -> Result<String, nerve_runtime::RuntimeError> {
    let Some(program) = windows_dialog_program() else {
        return Err(nerve_runtime::RuntimeError::adapter(
            "native save panel is unavailable on this Windows host",
        ));
    };
    run_output_command(
        program,
        &[
            "-NoProfile",
            "-STA",
            "-Command",
            WINDOWS_SAVE_FILE_SCRIPT,
            title,
            default_name,
        ],
        "PowerShell returned no save path",
        "file save cancelled",
    )
}

fn run_output_command(
    program: &str,
    args: &[&str],
    empty_message: &'static str,
    cancel_message: &'static str,
) -> Result<String, nerve_runtime::RuntimeError> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| nerve_runtime::RuntimeError::adapter(format!("run {program}: {err}")))?;
    if !output.status.success() {
        return Err(nerve_runtime::RuntimeError::adapter(command_failure(
            program,
            output.status,
            &output.stderr,
            cancel_message,
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\r', '\n'])
        .to_string();
    if text.is_empty() {
        return Err(nerve_runtime::RuntimeError::adapter(empty_message));
    }
    Ok(text)
}

fn run_status_command(program: &str, args: &[&str]) -> Result<(), nerve_runtime::RuntimeError> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|err| nerve_runtime::RuntimeError::adapter(format!("run {program}: {err}")))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(nerve_runtime::RuntimeError::adapter(format!(
                    "{program} exited with {status}"
                )))
            }
        })
}

fn command_failure(
    program: &str,
    status: std::process::ExitStatus,
    stderr: &[u8],
    cancel_message: &'static str,
) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if dialog_cancelled(status, &message) {
        return cancel_message.to_string();
    }
    if message.is_empty() {
        format!("{program} exited with {status}")
    } else {
        format!("{program} exited with {status}: {message}")
    }
}

fn dialog_cancelled(status: std::process::ExitStatus, message: &str) -> bool {
    let code = status.code();
    code == Some(2)
        || message.contains("User canceled")
        || message.contains("-128")
        || (message.is_empty() && code == Some(1))
}

pub(super) fn notifications_supported() -> bool {
    if cfg!(target_os = "macos") {
        return program_available("osascript");
    }
    cfg!(target_os = "linux") && program_available("notify-send")
}

pub(super) fn native_file_dialogs_supported() -> bool {
    if cfg!(target_os = "macos") {
        return program_available("osascript");
    }
    if cfg!(target_os = "windows") {
        return windows_dialog_program().is_some();
    }
    cfg!(target_os = "linux") && (program_available("zenity") || program_available("kdialog"))
}

pub(super) fn external_url_open_supported() -> bool {
    if cfg!(target_os = "macos") {
        return program_available("open");
    }
    if cfg!(target_os = "windows") {
        return windows_dialog_program().is_some();
    }
    cfg!(target_os = "linux") && (program_available("xdg-open") || program_available("gio"))
}

pub(super) fn windows_dialog_program() -> Option<&'static str> {
    ["powershell.exe", "powershell", "pwsh.exe", "pwsh"]
        .into_iter()
        .find(|program| program_available(program))
}

pub(super) fn host_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

/// Platform clipboard writer commands, ordered by preference.
pub(super) fn clipboard_write_commands() -> &'static [(&'static str, &'static [&'static str])] {
    const NO_ARGS: &[&str] = &[];
    const XCLIP_ARGS: &[&str] = &["-selection", "clipboard"];
    const XSEL_ARGS: &[&str] = &["--clipboard", "--input"];
    const MACOS: &[(&str, &[&str])] = &[("pbcopy", NO_ARGS)];
    const WINDOWS: &[(&str, &[&str])] = &[("clip", NO_ARGS)];
    const LINUX: &[(&str, &[&str])] = &[
        ("wl-copy", NO_ARGS),
        ("xclip", XCLIP_ARGS),
        ("xsel", XSEL_ARGS),
    ];
    if cfg!(target_os = "macos") {
        MACOS
    } else if cfg!(target_os = "windows") {
        WINDOWS
    } else {
        LINUX
    }
}

/// The OS file-manager opener for the current platform (used by `workspace.reveal`).
pub(super) fn workspace_opener() -> &'static str {
    if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    }
}
