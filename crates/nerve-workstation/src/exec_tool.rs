//! `run_command` — the agent's command-execution tool, as a ToolBox decorator.
//!
//! Follows the shipped `CheckpointToolBox` / `MemoryToolBox` pattern: it wraps an
//! inner [`ToolBox`] and adds one tool, `run_command`. Two safety properties are
//! structural here:
//!
//! 1. **Capability gate.** The tool is advertised *only* when `enabled` (the
//!    `--allow-exec` capability). A default agent never sees it and cannot
//!    execute. This decorator sits **inside** the P4 [`PolicyToolBox`] gate, so
//!    every `run_command` call is still authorized (Ask) before it runs — the
//!    gate stays the outermost decorator (north-star invariant 9).
//! 2. **No shell.** Arguments are argv only — `{ command, args[], cwd? }` — and
//!    are handed to the [`SandboxLauncher`] verbatim. nerve never builds a shell
//!    string from model input, so there is no interpolation/injection surface.
//!
//! Containment (cwd / env / timeout / output / net) is the launcher's job via the
//! [`SandboxPolicy`]; authorization is the gate's job. The two compose.

use crate::sandbox::{CommandSpec, SandboxLauncher, SandboxPolicy};
use nerve_agent::{AgentError, AgentResult, ToolBox, ToolSpec};
use nerve_core::CancelToken;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const RUN_COMMAND: &str = "run_command";

/// Decorator that adds the `run_command` tool over `inner`, running commands
/// through a [`SandboxLauncher`] under a base [`SandboxPolicy`].
pub(crate) struct ExecToolBox<T: ToolBox> {
    inner: T,
    launcher: Arc<dyn SandboxLauncher>,
    policy: SandboxPolicy,
    /// Whether `run_command` is exposed at all (the `--allow-exec` capability).
    enabled: bool,
}

impl<T: ToolBox> ExecToolBox<T> {
    pub(crate) fn new(
        inner: T,
        launcher: Arc<dyn SandboxLauncher>,
        policy: SandboxPolicy,
        enabled: bool,
    ) -> Self {
        Self {
            inner,
            launcher,
            policy,
            enabled,
        }
    }

    fn run(&self, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let args: RunCommandArgs = serde_json::from_value(args.clone())
            .map_err(|err| AgentError::Tool(format!("invalid run_command args: {err}")))?;
        let command = args.command.trim();
        if command.is_empty() {
            return Err(AgentError::Tool(
                "run_command requires a non-empty `command`".into(),
            ));
        }
        let policy = self
            .effective_policy(args.cwd.as_deref())
            .map_err(AgentError::Tool)?;
        let spec = CommandSpec {
            command: command.to_string(),
            args: args.args,
        };
        let output = self
            .launcher
            .launch(&spec, &policy, cancel)
            .map_err(|err| AgentError::Tool(format!("run_command failed: {err}")))?;
        // A cancel during the run kills the process; surface it as cancellation
        // rather than a (truncated) successful result.
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        Ok(json!({
            "exit_code": output.exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "timed_out": output.timed_out,
        }))
    }

    /// Clone the base policy, overriding the working directory when the call
    /// supplies a `cwd`. The override is contained to the workspace root.
    fn effective_policy(&self, cwd: Option<&str>) -> Result<SandboxPolicy, String> {
        let mut policy = self.policy.clone();
        if let Some(relative) = cwd {
            policy.cwd = resolve_cwd(&self.policy.cwd, relative)?;
        }
        Ok(policy)
    }
}

impl<T: ToolBox> ToolBox for ExecToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        if self.enabled {
            specs.push(run_command_spec());
        }
        specs
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if name != RUN_COMMAND {
            return self.inner.call(name, args, cancel);
        }
        if !self.enabled {
            return Err(AgentError::Tool(
                "run_command is not enabled (run nerve with --allow-exec)".into(),
            ));
        }
        self.run(args, cancel)
    }
}

/// Resolve a caller-supplied working directory against the workspace `root`,
/// rejecting anything that escapes it. Best-effort lexical containment (no `..`
/// traversal, no absolute path outside the root) — strong fs scoping is the
/// deferred Landlock backend's job.
fn resolve_cwd(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(relative);
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("cwd `{relative}` must not contain `..`"));
    }
    if candidate.is_absolute() {
        if candidate.starts_with(root) {
            return Ok(candidate.to_path_buf());
        }
        return Err(format!("cwd `{relative}` is outside the workspace root"));
    }
    Ok(root.join(candidate))
}

#[derive(Deserialize)]
struct RunCommandArgs {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
}

fn run_command_spec() -> ToolSpec {
    ToolSpec {
        name: RUN_COMMAND.to_string(),
        description: concat!(
            "Execute a local command (build / test / lint / scripts) and read its result. ",
            "Argv only — pass the program as `command` and each argument as a separate `args` ",
            "entry; there is NO shell, so `&&`, `|`, `>`, globs, and `$VARS` are NOT interpreted. ",
            "To chain steps, call this tool again. Runs in the workspace with a scrubbed ",
            "environment, no network, a wall-clock timeout, and capped output. Returns ",
            "{ exit_code, stdout, stderr, timed_out }. Each call requires permission."
        )
        .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Program to run, e.g. `cargo` or `/usr/bin/make`. No shell."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Arguments, each a separate string (e.g. [\"test\", \"--workspace\"])."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory, relative to the workspace root. Must stay within it."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{Output, RefuseLauncher};
    use std::sync::Mutex;

    struct FakeInner;

    impl ToolBox for FakeInner {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "read_file".into(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
            }]
        }

        fn call(&self, name: &str, args: &Value, _cancel: &CancelToken) -> AgentResult<Value> {
            Ok(json!({ "name": name, "args": args }))
        }
    }

    /// Records the argv it was handed and returns a canned success result, so
    /// tests can assert exactly what reached the containment boundary.
    struct RecordingLauncher {
        seen: Mutex<Option<(String, Vec<String>)>>,
    }

    impl RecordingLauncher {
        fn new() -> Self {
            Self {
                seen: Mutex::new(None),
            }
        }
    }

    impl SandboxLauncher for RecordingLauncher {
        fn launch(
            &self,
            spec: &CommandSpec,
            _policy: &SandboxPolicy,
            _cancel: &CancelToken,
        ) -> anyhow::Result<Output> {
            *self.seen.lock().expect("seen lock") = Some((spec.command.clone(), spec.args.clone()));
            Ok(Output {
                exit_code: Some(0),
                stdout: "ok".into(),
                stderr: String::new(),
                timed_out: false,
            })
        }
    }

    fn exec_box<T: ToolBox>(
        inner: T,
        launcher: Arc<dyn SandboxLauncher>,
        enabled: bool,
    ) -> ExecToolBox<T> {
        ExecToolBox::new(
            inner,
            launcher,
            SandboxPolicy::for_root(Some(Path::new("/work"))),
            enabled,
        )
    }

    #[test]
    fn run_command_absent_and_refused_when_disabled() {
        let tools = exec_box(FakeInner, Arc::new(RefuseLauncher), false);
        assert!(!tools.specs().iter().any(|spec| spec.name == RUN_COMMAND));
        let err = tools
            .call(
                RUN_COMMAND,
                &json!({ "command": "ls" }),
                &CancelToken::never(),
            )
            .expect_err("disabled exec must refuse");
        assert!(err.to_string().contains("not enabled"));
    }

    #[test]
    fn run_command_advertised_when_enabled() {
        let tools = exec_box(FakeInner, Arc::new(RecordingLauncher::new()), true);
        assert!(tools.specs().iter().any(|spec| spec.name == RUN_COMMAND));
    }

    #[test]
    fn argv_reaches_launcher_verbatim() {
        let launcher = Arc::new(RecordingLauncher::new());
        let tools = exec_box(FakeInner, launcher.clone(), true);
        let out = tools
            .call(
                RUN_COMMAND,
                &json!({ "command": "echo", "args": ["a && b", "$HOME"] }),
                &CancelToken::never(),
            )
            .expect("enabled exec runs");
        assert_eq!(out["exit_code"], 0);
        assert_eq!(out["stdout"], "ok");
        assert_eq!(out["timed_out"], false);
        let seen = launcher.seen.lock().expect("seen lock").clone();
        assert_eq!(
            seen,
            Some((
                "echo".to_string(),
                vec!["a && b".to_string(), "$HOME".to_string()]
            ))
        );
    }

    #[test]
    fn refuse_launcher_error_is_surfaced() {
        let tools = exec_box(FakeInner, Arc::new(RefuseLauncher), true);
        let err = tools
            .call(
                RUN_COMMAND,
                &json!({ "command": "ls" }),
                &CancelToken::never(),
            )
            .expect_err("refuse launcher must error");
        assert!(err.to_string().contains("unavailable"));
    }

    #[test]
    fn cwd_escape_is_rejected_before_launch() {
        let launcher = Arc::new(RecordingLauncher::new());
        let tools = exec_box(FakeInner, launcher.clone(), true);
        let err = tools
            .call(
                RUN_COMMAND,
                &json!({ "command": "ls", "cwd": "../etc" }),
                &CancelToken::never(),
            )
            .expect_err("parent-dir escape must be rejected");
        assert!(err.to_string().contains(".."));
        assert!(
            launcher.seen.lock().expect("seen lock").is_none(),
            "launcher must not run on a rejected cwd"
        );
    }

    #[test]
    fn empty_command_is_rejected() {
        let tools = exec_box(FakeInner, Arc::new(RecordingLauncher::new()), true);
        let err = tools
            .call(
                RUN_COMMAND,
                &json!({ "command": "   " }),
                &CancelToken::never(),
            )
            .expect_err("empty command must be rejected");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn cancelled_token_short_circuits_before_launch() {
        let launcher = Arc::new(RecordingLauncher::new());
        let tools = exec_box(FakeInner, launcher.clone(), true);
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = tools
            .call(RUN_COMMAND, &json!({ "command": "ls" }), &cancel)
            .expect_err("cancelled run must not launch");
        assert!(matches!(err, AgentError::Cancelled));
        assert!(launcher.seen.lock().expect("seen lock").is_none());
    }

    #[test]
    fn policy_gate_outermost_blocks_run_command_before_exec() {
        use crate::policy::{Policy, ToolGate};
        // Compose the production shape — PolicyToolBox(ExecToolBox(inner)) — and
        // assert the gate denies `run_command` (Ask, no approval) BEFORE the
        // launcher is consulted. Pins north-star invariant 9 for exec.
        let launcher = Arc::new(RecordingLauncher::new());
        let gated =
            ToolGate::deny(Policy::default()).wrap(exec_box(FakeInner, launcher.clone(), true));
        let err = gated
            .call(
                RUN_COMMAND,
                &json!({ "command": "ls" }),
                &CancelToken::never(),
            )
            .expect_err("gate must deny run_command");
        assert!(err.to_string().contains("permission denied"));
        assert!(
            launcher.seen.lock().expect("seen lock").is_none(),
            "launcher must never run when the gate denies"
        );
    }

    #[test]
    fn non_exec_calls_delegate_to_inner() {
        let tools = exec_box(FakeInner, Arc::new(RefuseLauncher), true);
        let out = tools
            .call("read_file", &json!({ "path": "x" }), &CancelToken::never())
            .expect("delegated to inner");
        assert_eq!(out["name"], "read_file");
    }
}
