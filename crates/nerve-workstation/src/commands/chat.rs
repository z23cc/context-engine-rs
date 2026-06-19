//! `nerve chat` — thin launcher for the bundled `nerve-chat` TUI client.
//!
//! The terminal UI is a runtime-protocol *client*, not the engine: it ships as a
//! separate executable (`nerve-chat`, a self-contained bun build) and speaks to
//! the engine only over the daemon's stdio protocol. This command resolves the
//! provider/model (flag -> saved default -> first-run picker), then locates that
//! binary and hands control to it — engine and client stay decoupled (north-star:
//! client surfaces ride the protocol, never the kernel).

use anyhow::{Result, anyhow};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Args)]
pub(crate) struct ChatArgs {
    /// Model provider (`claude`/`chatgpt`/`xai`). Falls back to the saved default,
    /// then a first-run interactive picker.
    #[arg(long)]
    provider: Option<String>,
    /// Model id (e.g. `claude-sonnet-4`/`gpt-5.5`/`grok-4-fast`). Falls back to
    /// the saved default, then the interactive picker.
    #[arg(long)]
    model: Option<String>,
    /// Project root the daemon operates on (defaults to the current directory).
    #[arg(long)]
    root: Option<PathBuf>,
    /// Named agent / skill definition to start the session with.
    #[arg(long)]
    agent: Option<String>,
    /// Engine binary used to spawn the daemon (defaults to this `nerve`).
    #[arg(long)]
    binary: Option<PathBuf>,
}

/// Resolve provider/model (flag -> saved default -> picker), locate `nerve-chat`,
/// and hand off to it. The engine binary is passed explicitly so the client
/// spawns the matching daemon.
pub(crate) fn chat(args: ChatArgs) -> Result<()> {
    let (provider, model) = crate::runconfig::resolve(args.provider, args.model, true)?;
    let binary = locate_chat_binary();
    let mut command = Command::new(&binary);
    let engine = match args.binary {
        Some(path) => Some(path),
        None => std::env::current_exe().ok(),
    };
    if let Some(engine) = engine {
        command.arg("--binary").arg(engine);
    }
    command
        .arg("--provider")
        .arg(&provider)
        .arg("--model")
        .arg(&model);
    if let Some(root) = &args.root {
        command.arg("--root").arg(root);
    }
    if let Some(agent) = &args.agent {
        command.arg("--agent").arg(agent);
    }
    handoff(command, &binary)
}

/// Resolution order: `NERVE_CHAT_BIN` -> sibling of the running `nerve` (Homebrew
/// installs both into the same `bin/`) -> bare name for a `PATH` search.
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
