//! `nerve chat` — thin launcher for the bundled `nerve-chat` TUI client.
//!
//! The terminal UI is a runtime-protocol *client*, not the engine: it ships as a
//! separate executable (`nerve-chat`, a self-contained bun build) and speaks to
//! the engine only over the daemon's stdio protocol. This command merely locates
//! that binary and hands control to it, so the engine and the client stay
//! decoupled (north-star: client surfaces ride the protocol, never the kernel).

use anyhow::{Result, anyhow};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    /// Arguments forwarded verbatim to the bundled `nerve-chat` client, e.g.
    /// `--provider anthropic --model claude-sonnet-4 [--root PATH] [--agent NAME]`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) forwarded: Vec<String>,
}

/// Locate `nerve-chat` and hand off to it. Prepends `--binary <this nerve>` so the
/// client spawns the matching engine; any user-supplied `--binary` is forwarded
/// after and wins (the client keeps the last occurrence).
pub(crate) fn chat(args: ChatArgs) -> Result<()> {
    let binary = locate_chat_binary();
    let mut command = Command::new(&binary);
    if let Ok(current) = std::env::current_exe() {
        command.arg("--binary").arg(current);
    }
    command.args(&args.forwarded);
    handoff(command, &binary)
}

/// Resolution order: `NERVE_CHAT_BIN` → sibling of the running `nerve` (Homebrew
/// installs both into the same `bin/`) → bare name for a `PATH` search.
fn locate_chat_binary() -> PathBuf {
    if let Ok(path) = std::env::var("NERVE_CHAT_BIN") {
        let candidate = PathBuf::from(path);
        if candidate.is_file() {
            return candidate;
        }
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let candidate = dir.join(chat_binary_name());
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(chat_binary_name())
}

fn chat_binary_name() -> &'static str {
    if cfg!(windows) {
        "nerve-chat.exe"
    } else {
        "nerve-chat"
    }
}

/// On Unix, replace this process with the client so it owns the tty and signals
/// (Ctrl-C reaches the chat loop directly). On other platforms, spawn it and
/// forward the exit code. A missing binary becomes an actionable error.
#[cfg(unix)]
fn handoff(mut command: Command, binary: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    // `exec` only returns if it failed to replace the image.
    Err(missing_binary_error(binary, command.exec()))
}

#[cfg(not(unix))]
fn handoff(mut command: Command, binary: &Path) -> Result<()> {
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => Err(missing_binary_error(binary, err)),
    }
}

fn missing_binary_error(binary: &Path, err: std::io::Error) -> anyhow::Error {
    anyhow!(
        "could not launch the chat client `{}`: {err}\n\
         `nerve-chat` ships in the macOS bottle. Build it from source with \
         `bun build src/cli/chat.ts --compile --outfile dist/nerve-chat` in `packages/tui`, \
         or point NERVE_CHAT_BIN at the binary.",
        binary.display()
    )
}
