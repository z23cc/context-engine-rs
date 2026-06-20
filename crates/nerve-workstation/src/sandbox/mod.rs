//! `SandboxLauncher` — the containment seam for agent command execution.
//!
//! Execution is the agent's highest-risk capability, so the *containment* story
//! is a first-class seam rather than an inline `Command::spawn`. A
//! [`SandboxLauncher`] answers a single question — *"what may this process
//! reach?"* (cwd, filesystem, network, environment, time, output size) — and is
//! deliberately independent of the P4 authorization gate (`policy.rs`), which
//! answers *"is the caller allowed to run this at all?"*. The two compose: a
//! call must pass the gate **and** then run contained.
//!
//! This lives entirely in `nerve-workstation` (never `nerve-core`): execution is
//! non-deterministic, so it stays out of the golden-tested kernel. The port is
//! pure data plus one trait, so a stronger backend can land without touching any
//! caller. See `docs/designs/agent-exec-sandbox.md`.
//!
//! Backends: [`ProcessLauncher`] (MVP — best-effort "trusted local dev"
//! containment) and [`RefuseLauncher`] (daemon / remote — refuses outright).
//!
//! Deferred (future seams, intentionally NOT here): Linux Landlock/seccomp and
//! cgroups, macOS App-Sandbox bundle, microVM/WASM isolation, shell-mode
//! (pipes/redirects), and the daemon approval round-trip. The unused
//! `fs_read`/`fs_write`/`net` policy fields below are the data contract those
//! strong backends will enforce — carried now so the seam is stable.

// The containment policy is the seam's stable data contract: several fields
// (filesystem scoping, network posture) are consumed only by the deferred
// strong backends, not the MVP `ProcessLauncher`. Mirrors `checkpoint.rs` /
// `memory.rs`, which also predeclare their not-yet-wired surface.
#![allow(dead_code)]

mod persistent;
mod process;

pub(crate) use persistent::PersistentChild;
pub(crate) use process::ProcessLauncher;

use anyhow::{Result, anyhow};
use nerve_core::CancelToken;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default hard wall-clock timeout for one command.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Default cap on each captured stream (stdout / stderr), in bytes.
pub(crate) const DEFAULT_MAX_OUTPUT: usize = 32 * 1024;

/// One command to run: a program plus its argument vector. There is **no shell**
/// and never a single command string — the caller passes argv directly, so there
/// is no interpolation/injection surface (chaining is N separate calls).
pub(crate) struct CommandSpec {
    /// Program to execute (resolved via the child `PATH`, or an absolute path).
    pub(crate) command: String,
    /// Arguments passed verbatim — metacharacters are literal, never expanded.
    pub(crate) args: Vec<String>,
}

/// Network containment posture. The MVP [`ProcessLauncher`] records intent but
/// cannot enforce it; a strong backend (Landlock + seccomp) is what actually
/// denies the network.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NetPolicy {
    /// No network access (default).
    Deny,
    /// Network allowed (e.g. dependency fetches) — opt-in, future config.
    Allow,
}

/// Environment containment: an allowlist over the parent environment. The
/// launcher additionally applies an unconditional **secret scrub** on top of the
/// allowlist (see `process::is_secret_name`), so a broadly-allowed prefix can
/// never leak a `*_TOKEN` / `*_KEY` / `*_SECRET` into the child.
#[derive(Clone)]
pub(crate) struct EnvPolicy {
    /// Exact variable names passed through from the parent (e.g. `PATH`, `HOME`).
    pub(crate) allow: Vec<String>,
    /// Name prefixes passed through (e.g. `CARGO`, `RUST`, `NODE`) so common
    /// toolchains work without enumerating every variable.
    pub(crate) allow_prefixes: Vec<String>,
}

impl EnvPolicy {
    /// A pragmatic allowlist for the *trusted local developer* audience: enough
    /// to make `cargo` / `npm` / `go` / `python` builds work, with secrets still
    /// scrubbed by the launcher. Tighten via config in a later phase.
    pub(crate) fn dev_default() -> Self {
        let allow = [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "LANG",
            "TERM",
            "TMPDIR",
            "TMP",
            "TEMP",
            "TZ",
            "COLORTERM",
            "TERMINFO",
        ];
        // Deliberately omits credential-prone prefixes (`NPM_CONFIG`, `PIP_`):
        // those carry registry auth tokens and index URLs with embedded
        // `user:pass`, which a name-based scrub cannot fully sanitize. npm / pip
        // still work via their config files (reached through HOME).
        let allow_prefixes = [
            "LC_",
            "CARGO",
            "RUSTUP",
            "RUST_",
            "NODE",
            "PNPM",
            "YARN",
            "BUN_",
            "GOPATH",
            "GOROOT",
            "GO1",
            "GOCACHE",
            "GOMOD",
            "JAVA_",
            "JDK_",
            "MAVEN_",
            "GRADLE_",
            "PYENV",
            "PYTHON",
            "VIRTUAL_ENV",
            "CONDA",
        ];
        Self {
            allow: allow.into_iter().map(str::to_string).collect(),
            allow_prefixes: allow_prefixes.into_iter().map(str::to_string).collect(),
        }
    }

    /// Whether `name` is permitted by the allowlist (exact name or known prefix).
    /// The launcher still rejects secret-shaped names on top of this.
    pub(crate) fn allows(&self, name: &str) -> bool {
        self.allow.iter().any(|allowed| allowed == name)
            || self
                .allow_prefixes
                .iter()
                .any(|prefix| name.starts_with(prefix.as_str()))
    }
}

/// Pure-data containment policy derived from the workspace root and config, then
/// handed to a [`SandboxLauncher`]. Mirrors nerve's port culture
/// (`CatalogProvider` / `LlmProvider` / `MemoryStore`): the policy is data; the
/// backend is the behaviour.
#[derive(Clone)]
pub(crate) struct SandboxPolicy {
    /// Working directory the command runs in (defaults to the workspace root).
    pub(crate) cwd: PathBuf,
    /// Paths the command may read (workspace + toolchain dirs). Enforced only by
    /// strong backends; carried as the containment contract.
    pub(crate) fs_read: Vec<PathBuf>,
    /// Paths the command may write (workspace only). Enforced only by strong
    /// backends; carried as the containment contract.
    pub(crate) fs_write: Vec<PathBuf>,
    /// Network posture (Deny by default).
    pub(crate) net: NetPolicy,
    /// Environment allowlist (secrets scrubbed by the launcher).
    pub(crate) env: EnvPolicy,
    /// Hard wall-clock kill after this duration.
    pub(crate) timeout: Duration,
    /// Per-stream output cap in bytes.
    pub(crate) max_output: usize,
}

impl SandboxPolicy {
    /// Build the default policy for a workspace `root`: cwd and fs scope are the
    /// root, network is denied, the environment is the dev allowlist, and the
    /// time/output bounds are the defaults. With no root, falls back to the
    /// process current directory (still bounded, just not workspace-scoped).
    pub(crate) fn for_root(root: Option<&Path>) -> Self {
        let cwd = root
            .map(Path::to_path_buf)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            fs_read: vec![cwd.clone()],
            fs_write: vec![cwd.clone()],
            cwd,
            net: NetPolicy::Deny,
            env: EnvPolicy::dev_default(),
            timeout: DEFAULT_TIMEOUT,
            max_output: DEFAULT_MAX_OUTPUT,
        }
    }
}

/// The result of a contained run. `exit_code` is `None` when the process was
/// terminated by a signal (e.g. the wall-clock timeout kill), which has no exit
/// code; `timed_out` distinguishes that case.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) timed_out: bool,
}

/// The containment seam: run a command under a [`SandboxPolicy`].
///
/// Implementations bound the blast radius to whatever the policy permits and the
/// backend can enforce. The MVP [`ProcessLauncher`] is best-effort; stronger
/// backends (Landlock/seccomp, microVM) slot in here without changing callers.
pub(crate) trait SandboxLauncher: Send + Sync {
    /// Run `spec` under `policy`, honoring `cancel` (a cancelled token aborts a
    /// running command, like the wall-clock timeout).
    fn launch(
        &self,
        spec: &CommandSpec,
        policy: &SandboxPolicy,
        cancel: &CancelToken,
    ) -> Result<Output>;

    /// Run `spec` like [`launch`](Self::launch), additionally feeding `stdin` to
    /// the child and invoking `on_line` for each newline-terminated stdout line as
    /// it arrives. This is the streaming variant used by the delegate runtime to
    /// surface an external agent's progress live (DA-2): the agent's stdout is a
    /// JSONL/stream-json event stream, so a per-line callback is the natural seam.
    ///
    /// The default implementation has no true streaming — it runs the blocking
    /// [`launch`](Self::launch) (which ignores `stdin`) and then replays the
    /// captured stdout line-by-line through `on_line`, so a non-streaming backend
    /// still drives the same parser. [`ProcessLauncher`] overrides this with real
    /// incremental line delivery and stdin write; [`RefuseLauncher`] refuses.
    fn launch_streaming(
        &self,
        spec: &CommandSpec,
        policy: &SandboxPolicy,
        stdin: &str,
        cancel: &CancelToken,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<Output> {
        let _ = stdin;
        let output = self.launch(spec, policy, cancel)?;
        for line in output.stdout.lines() {
            on_line(line);
        }
        Ok(output)
    }

    /// Spawn a **long-lived** contained child and hand back a [`PersistentChild`]
    /// handle: an open stdin writer (so the caller can feed multiple messages over
    /// time) plus a stdout line stream. Unlike [`launch_streaming`](Self::launch_streaming),
    /// this does **not** wait for the process to exit — the child stays alive across
    /// turns and exits when its stdin reaches EOF (the persistent steerable delegate
    /// session, DA-5a). The same containment (forced cwd, scrubbed env, isolated
    /// process group) applies; the wall-clock timeout does **not**, because a live
    /// session is bounded by explicit close/cancel, not a single command's clock.
    ///
    /// The default implementation refuses: a launcher that cannot stream a real
    /// subprocess cannot host a live session. [`ProcessLauncher`] overrides this;
    /// [`RefuseLauncher`] inherits the refusal.
    fn launch_persistent(
        &self,
        spec: &CommandSpec,
        policy: &SandboxPolicy,
    ) -> Result<PersistentChild> {
        let _ = (spec, policy);
        Err(anyhow!(
            "persistent sessions are unavailable in this context (no contained sandbox backend)"
        ))
    }
}

/// A launcher that refuses every command — the safe default for any context
/// without a trusted contained backend (the daemon, and future remote/session
/// runs). Execution capability is bound to the **trust context**, not merely a
/// flag: a daemon-served run selects this launcher and cannot execute even if
/// the capability flag were somehow set.
pub(crate) struct RefuseLauncher;

impl SandboxLauncher for RefuseLauncher {
    fn launch(
        &self,
        _spec: &CommandSpec,
        _policy: &SandboxPolicy,
        _cancel: &CancelToken,
    ) -> Result<Output> {
        Err(anyhow!(
            "command execution is unavailable in this context (no contained sandbox backend)"
        ))
    }
}

/// The best-effort trusted-local launcher as a trait object — selected by the
/// interactive CLI composition root.
pub(crate) fn process_launcher() -> Arc<dyn SandboxLauncher> {
    Arc::new(ProcessLauncher)
}

/// The refuse-everything launcher as a trait object — selected by the daemon and
/// any non-interactive / remote composition root.
pub(crate) fn refuse_launcher() -> Arc<dyn SandboxLauncher> {
    Arc::new(RefuseLauncher)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuse_launcher_always_errors() {
        let spec = CommandSpec {
            command: "echo".into(),
            args: vec!["hi".into()],
        };
        let policy = SandboxPolicy::for_root(None);
        let err = RefuseLauncher
            .launch(&spec, &policy, &CancelToken::never())
            .expect_err("refuse launcher must error");
        assert!(err.to_string().contains("unavailable"));
    }

    #[test]
    fn refuse_launcher_refuses_streaming_too() {
        // The streaming variant must also refuse (its default impl calls `launch`,
        // which errors), so a delegate spawn cannot bypass the refusal by asking
        // for the streaming path.
        let spec = CommandSpec {
            command: "echo".into(),
            args: Vec::new(),
        };
        let policy = SandboxPolicy::for_root(None);
        let mut lines = Vec::new();
        let err = RefuseLauncher
            .launch_streaming(&spec, &policy, "task", &CancelToken::never(), &mut |line| {
                lines.push(line.to_string());
            })
            .expect_err("refuse launcher must error on streaming");
        assert!(err.to_string().contains("unavailable"));
        assert!(lines.is_empty(), "no lines from a refused stream");
    }

    /// A trivial launcher whose `launch` returns canned multi-line stdout, used to
    /// prove the trait's default `launch_streaming` replays it line-by-line.
    struct CannedReplay;

    impl SandboxLauncher for CannedReplay {
        fn launch(
            &self,
            _spec: &CommandSpec,
            _policy: &SandboxPolicy,
            _cancel: &CancelToken,
        ) -> Result<Output> {
            Ok(Output {
                exit_code: Some(0),
                stdout: "one\ntwo\nthree\n".into(),
                stderr: String::new(),
                timed_out: false,
            })
        }
    }

    #[test]
    fn default_streaming_replays_captured_lines() {
        let spec = CommandSpec {
            command: "x".into(),
            args: Vec::new(),
        };
        let policy = SandboxPolicy::for_root(None);
        let mut lines = Vec::new();
        let output = CannedReplay
            .launch_streaming(&spec, &policy, "", &CancelToken::never(), &mut |line| {
                lines.push(line.to_string());
            })
            .expect("canned stream");
        assert_eq!(lines, vec!["one", "two", "three"]);
        assert_eq!(output.exit_code, Some(0));
    }

    #[test]
    fn for_root_defaults_are_bounded_and_deny_net() {
        let root = PathBuf::from("/tmp/project");
        let policy = SandboxPolicy::for_root(Some(&root));
        assert_eq!(policy.cwd, root);
        assert_eq!(policy.fs_write, vec![root.clone()]);
        assert_eq!(policy.net, NetPolicy::Deny);
        assert_eq!(policy.timeout, DEFAULT_TIMEOUT);
        assert_eq!(policy.max_output, DEFAULT_MAX_OUTPUT);
    }

    #[test]
    fn env_allowlist_matches_exact_names_and_prefixes() {
        let env = EnvPolicy::dev_default();
        assert!(env.allows("PATH"));
        assert!(env.allows("HOME"));
        assert!(env.allows("CARGO_HOME")); // prefix CARGO
        assert!(env.allows("RUSTUP_HOME")); // prefix RUSTUP
        assert!(env.allows("LC_ALL")); // prefix LC_
        assert!(!env.allows("RANDOM_VAR"));
        assert!(!env.allows("AWS_PROFILE"));
    }
}
