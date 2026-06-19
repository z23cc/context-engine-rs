//! Locating the `nerve` binary and building the daemon command.
//!
//! Mirrors the TS client's `defaultBinary`: walk up from the cwd looking for
//! `target/debug/nerve`, else fall back to the bare name for a `PATH` search.
//! Tests inject a fully-formed command via [`DaemonSpec::command`].

use std::path::{Path, PathBuf};
use tokio::process::Command;

/// How to launch the daemon. Either an explicit binary + root (the normal path)
/// or a pre-built command (used by tests / callers that already resolved one).
#[derive(Debug, Clone)]
pub struct DaemonSpec {
    /// Path/name of the engine binary that hosts `daemon --stdio`.
    pub binary: PathBuf,
    /// Absolute project root the daemon operates on (becomes `--root`).
    pub root: PathBuf,
    /// Optional `--provider` passed through to the daemon.
    pub provider: Option<String>,
    /// Optional `--model` passed through to the daemon.
    pub model: Option<String>,
    /// Extra args appended after the standard ones.
    pub extra_args: Vec<String>,
}

impl DaemonSpec {
    /// Build a spec, defaulting the binary to a discovered `nerve`.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self {
            binary: default_binary(),
            root,
            provider: None,
            model: None,
            extra_args: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_binary(mut self, binary: PathBuf) -> Self {
        self.binary = binary;
        self
    }

    #[must_use]
    pub fn with_provider_model(mut self, provider: Option<String>, model: Option<String>) -> Self {
        self.provider = provider;
        self.model = model;
        self
    }

    /// Build the `tokio` command: `nerve daemon --stdio --root <abs> [...]`.
    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.binary);
        command.arg("daemon").arg("--stdio");
        command.arg("--root").arg(&self.root);
        if let Some(provider) = &self.provider {
            command.arg("--provider").arg(provider);
        }
        if let Some(model) = &self.model {
            command.arg("--model").arg(model);
        }
        for arg in &self.extra_args {
            command.arg(arg);
        }
        command
    }
}

/// Discover the `nerve` binary: walk up from cwd looking for `target/debug/nerve`
/// (the in-repo dev build), else the bare name. Mirrors the TS `defaultBinary`.
fn default_binary() -> PathBuf {
    let name = binary_name();
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir: &Path = &cwd;
        loop {
            let candidate = dir.join("target").join("debug").join(name);
            if candidate.is_file() {
                return candidate;
            }
            match dir.parent() {
                Some(parent) => dir = parent,
                None => break,
            }
        }
    }
    PathBuf::from(name)
}

fn binary_name() -> &'static str {
    if cfg!(windows) { "nerve.exe" } else { "nerve" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_includes_root_and_stdio() {
        let spec =
            DaemonSpec::new(PathBuf::from("/tmp/project")).with_binary(PathBuf::from("nerve"));
        let command = spec.command();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "daemon");
        assert!(args.contains(&"--stdio".to_string()));
        let root_pos = args.iter().position(|a| a == "--root").expect("root flag");
        assert_eq!(args[root_pos + 1], "/tmp/project");
    }

    #[test]
    fn command_passes_provider_and_model() {
        let spec = DaemonSpec::new(PathBuf::from("/tmp/project"))
            .with_binary(PathBuf::from("nerve"))
            .with_provider_model(Some("claude".to_string()), Some("opus".to_string()));
        let command = spec.command();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let provider_pos = args
            .iter()
            .position(|a| a == "--provider")
            .expect("provider");
        assert_eq!(args[provider_pos + 1], "claude");
        let model_pos = args.iter().position(|a| a == "--model").expect("model");
        assert_eq!(args[model_pos + 1], "opus");
    }
}
