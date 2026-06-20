//! `delegate_agent` — the chat agent's external-agent delegation tool, as a
//! ToolBox decorator (DA-3).
//!
//! DA-2 wired delegation as a daemon **job** (`delegate.start`); this module adds
//! the agent-facing **tool** path so the LLM in a chat session can hand a subtask
//! to an external coding-agent CLI (codex / claude / gemini) mid-turn. It follows
//! the [`ExecToolBox`](crate::exec_tool::ExecToolBox) template — wrap an inner
//! [`ToolBox`], add one tool when enabled, route it through the
//! [`SandboxLauncher`] — and reuses DA-2's runtime
//! ([`build_command`](crate::delegate_runtime::build_command) /
//! [`DelegateParser`] / [`DelegateOutcome`](crate::delegate_runtime)) verbatim, so
//! the job path and the tool path produce identical argv and parse identically.
//!
//! Three safety properties are structural here, mirroring `ExecToolBox`:
//!
//! 1. **Capability gate.** The tool is advertised *only* when `enabled`
//!    (the daemon's `--allow-delegate` lift, threaded into the run config). A
//!    default agent never sees it.
//! 2. **Top-level only (recursion guard).** Only the top-level agent delegates;
//!    `enabled` is set by the composition root *only* at sub-agent depth 0, so a
//!    delegated/sub-agent context can never recursively spawn more external
//!    agents (RepoPrompt's top-level-only rule).
//! 3. **Gate still authorizes.** This decorator sits **inside** the P4
//!    [`PolicyToolBox`](crate::policy) gate, so every `delegate_agent` call is
//!    still authorized (exec-tier → Ask) before it runs — the gate stays the
//!    outermost decorator (north-star invariant 9).
//!
//! Containment (forced cwd, scrubbed env, allowed network for the child's LLM
//! API, long timeout) is [`delegate_policy`](crate::delegate_runtime::delegate_policy)'s
//! job; authorization is the gate's job. The child authenticates with its **own**
//! on-disk login — nerve's credentials are never inherited (the env scrub strips
//! them).

use crate::delegate_runtime::{self, DelegateAgent, DelegateError, DelegateParser};
use crate::sandbox::SandboxLauncher;
use nerve_agent::{AgentError, AgentResult, ToolBox, ToolSpec};
use nerve_core::CancelToken;
use nerve_runtime::DelegateAutonomy;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const DELEGATE_AGENT: &str = "delegate_agent";

/// Sink for live delegate progress, called once per human-meaningful stream line
/// with `(agent, text)`. The composition root closes over the run's scope id and
/// the runtime event emitter, so this module stays unaware of the protocol event
/// shape (it emits `RuntimeEvent::delegate_progress` itself). `None` (the CLI
/// path) simply drops progress — the final outcome is still parsed and returned.
pub(crate) type DelegateProgressSink = dyn Fn(&str, &str) + Send + Sync;

/// Decorator that adds the `delegate_agent` tool over `inner`, spawning external
/// coding-agent CLIs through a [`SandboxLauncher`] and streaming their progress.
pub(crate) struct DelegateAgentToolBox<T: ToolBox> {
    inner: T,
    launcher: Arc<dyn SandboxLauncher>,
    /// Workspace root a delegated run is confined to; `cwd` args resolve under it.
    root: Option<PathBuf>,
    /// Whether `delegate_agent` is exposed at all. Set by the composition root to
    /// `allow_delegate && depth == 0` — the capability lift *and* the top-level
    /// recursion guard fold into this one flag.
    enabled: bool,
    /// Optional live-progress sink (session/agent-run path); `None` on the CLI.
    progress: Option<Arc<DelegateProgressSink>>,
}

impl<T: ToolBox> DelegateAgentToolBox<T> {
    pub(crate) fn new(
        inner: T,
        launcher: Arc<dyn SandboxLauncher>,
        root: Option<PathBuf>,
        enabled: bool,
        progress: Option<Arc<DelegateProgressSink>>,
    ) -> Self {
        Self {
            inner,
            launcher,
            root,
            enabled,
            progress,
        }
    }

    fn run(&self, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let args: DelegateArgs = serde_json::from_value(args.clone())
            .map_err(|err| AgentError::Tool(format!("invalid delegate_agent args: {err}")))?;
        let task = args.task.trim();
        if task.is_empty() {
            return Err(AgentError::Tool(
                "delegate_agent requires a non-empty `task`".into(),
            ));
        }
        let agent = DelegateAgent::from_name(&args.agent).map_err(delegate_tool_error)?;
        // Delegation must confine the child to a concrete root; without one there
        // is nothing to scope `cwd` against, so refuse rather than run unconfined.
        let root = self.root.as_deref().ok_or_else(|| {
            AgentError::Tool("delegate_agent requires a served workspace root".into())
        })?;
        let cwd = delegate_runtime::resolve_delegate_cwd(root, args.cwd.as_deref())
            .map_err(delegate_tool_error)?;
        let invocation = delegate_runtime::build_command(
            agent,
            task,
            &cwd,
            args.autonomy,
            args.model.as_deref(),
        );
        let mut policy = delegate_runtime::delegate_policy(&cwd);
        if let Some(secs) = args.timeout_secs {
            policy.timeout = Duration::from_secs(secs);
        }
        let mut parser = DelegateParser::new(agent);
        let progress = self.progress.clone();
        let agent_name = args.agent.clone();
        let mut on_line = |line: &str| {
            if let Some(text) = parser.ingest(line)
                && let Some(sink) = &progress
            {
                sink(&agent_name, &text);
            }
        };
        let output = self
            .launcher
            .launch_streaming(
                &invocation.spec,
                &policy,
                &invocation.stdin,
                cancel,
                &mut on_line,
            )
            .map_err(|err| delegate_launch_error(&args.agent, &err))?;
        // A cancel during the run kills the child; surface it as cancellation
        // rather than a partial "success".
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let outcome = parser.finish(&args.agent, output.exit_code, output.timed_out);
        Ok(outcome.to_json())
    }
}

impl<T: ToolBox> ToolBox for DelegateAgentToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        if self.enabled {
            specs.push(delegate_agent_spec());
        }
        specs
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if name != DELEGATE_AGENT {
            return self.inner.call(name, args, cancel);
        }
        if !self.enabled {
            return Err(AgentError::Tool(
                "delegation disabled — start with --allow-delegate".into(),
            ));
        }
        self.run(args, cancel)
    }
}

/// Map a delegate-runtime caller error (unknown agent / cwd escape) to a tool error.
fn delegate_tool_error(err: DelegateError) -> AgentError {
    AgentError::Tool(err.to_string())
}

/// Render a launcher failure for a delegated spawn. A refusing launcher (the
/// default trust context) produces the disabled message that points at the lift
/// flag; any other spawn failure is surfaced verbatim.
fn delegate_launch_error(agent: &str, err: &anyhow::Error) -> AgentError {
    if err.to_string().contains("no contained sandbox backend") {
        return AgentError::Tool("delegation disabled — start with --allow-delegate".into());
    }
    AgentError::Tool(format!("delegate `{agent}` failed: {err}"))
}

#[derive(Deserialize)]
struct DelegateArgs {
    agent: String,
    task: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    autonomy: DelegateAutonomy,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

fn delegate_agent_spec() -> ToolSpec {
    ToolSpec {
        name: DELEGATE_AGENT.to_string(),
        description: concat!(
            "Delegate a subtask to an external coding-agent CLI (codex, claude, or gemini) ",
            "spawned as a child process, and read back its result. Use this to hand off a ",
            "self-contained investigation or change to another agent. READ-ONLY by default ",
            "(`autonomy: read_only`): the child may only read the workspace — pass ",
            "`autonomy: edit` to let it modify files or `autonomy: full` to also let it run ",
            "commands. The child runs in the workspace with network access (it calls its own ",
            "LLM) and authenticates with its OWN on-disk login; nerve's credentials are never ",
            "shared. Each call is permission-gated. Returns ",
            "{ agent, ok, result, exit_code, usage, cost_usd, timed_out }."
        )
        .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["codex", "claude", "gemini"],
                    "description": "Which external coding-agent CLI to delegate to."
                },
                "task": {
                    "type": "string",
                    "description": "The subtask for the delegated agent to perform."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory, relative to the workspace root. Must stay within it."
                },
                "autonomy": {
                    "type": "string",
                    "enum": ["read_only", "edit", "full"],
                    "description": "Permission level granted to the child (default read_only)."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model id override for the delegated agent."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Optional wall-clock timeout in seconds (default 600)."
                }
            },
            "required": ["agent", "task"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegate_runtime::DEFAULT_DELEGATE_TIMEOUT;
    use crate::sandbox::{CommandSpec, Output, RefuseLauncher, SandboxPolicy};
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

    /// Records the spec it was handed and replays canned stdout through the
    /// trait's default line-replay streaming path, so tests can assert exactly
    /// what argv reached the boundary and exercise the parser without a process.
    struct RecordingLauncher {
        stdout: String,
        seen: Mutex<Option<(String, Vec<String>)>>,
        seen_timeout: Mutex<Option<Duration>>,
    }

    impl RecordingLauncher {
        fn new(stdout: &str) -> Self {
            Self {
                stdout: stdout.to_string(),
                seen: Mutex::new(None),
                seen_timeout: Mutex::new(None),
            }
        }
    }

    impl SandboxLauncher for RecordingLauncher {
        fn launch(
            &self,
            spec: &CommandSpec,
            policy: &SandboxPolicy,
            _cancel: &CancelToken,
        ) -> anyhow::Result<Output> {
            *self.seen.lock().expect("seen lock") = Some((spec.command.clone(), spec.args.clone()));
            *self.seen_timeout.lock().expect("timeout lock") = Some(policy.timeout);
            Ok(Output {
                exit_code: Some(0),
                stdout: self.stdout.clone(),
                stderr: String::new(),
                timed_out: false,
            })
        }
    }

    fn delegate_box<T: ToolBox>(
        inner: T,
        launcher: Arc<dyn SandboxLauncher>,
        enabled: bool,
        progress: Option<Arc<DelegateProgressSink>>,
    ) -> DelegateAgentToolBox<T> {
        DelegateAgentToolBox::new(
            inner,
            launcher,
            Some(PathBuf::from("/work")),
            enabled,
            progress,
        )
    }

    #[test]
    fn delegate_absent_and_refused_when_disabled() {
        let tools = delegate_box(FakeInner, Arc::new(RefuseLauncher), false, None);
        assert!(!tools.specs().iter().any(|spec| spec.name == DELEGATE_AGENT));
        let err = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "x" }),
                &CancelToken::never(),
            )
            .expect_err("disabled delegate must refuse");
        assert!(err.to_string().contains("--allow-delegate"));
    }

    #[test]
    fn delegate_absent_at_subagent_depth() {
        // The composition root sets `enabled = allow_delegate && depth == 0`. At
        // depth > 0 (a delegated/sub-agent context) the flag is false, so the tool
        // is absent — the top-level-only recursion guard.
        let tools = delegate_box(FakeInner, Arc::new(RecordingLauncher::new("")), false, None);
        assert!(!tools.specs().iter().any(|spec| spec.name == DELEGATE_AGENT));
    }

    #[test]
    fn delegate_advertised_when_enabled() {
        let tools = delegate_box(FakeInner, Arc::new(RecordingLauncher::new("")), true, None);
        let spec = tools
            .specs()
            .into_iter()
            .find(|spec| spec.name == DELEGATE_AGENT)
            .expect("delegate_agent advertised");
        // The schema enumerates the three agents and requires agent + task.
        assert_eq!(spec.input_schema["required"], json!(["agent", "task"]));
        assert_eq!(spec.input_schema["additionalProperties"], json!(false));
        assert_eq!(
            spec.input_schema["properties"]["agent"]["enum"],
            json!(["codex", "claude", "gemini"])
        );
    }

    #[test]
    fn codex_argv_read_only_reaches_launcher() {
        let launcher = Arc::new(RecordingLauncher::new(""));
        let tools = delegate_box(FakeInner, launcher.clone(), true, None);
        tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "investigate" }),
                &CancelToken::never(),
            )
            .expect("enabled delegate runs");
        let seen = launcher.seen.lock().expect("seen lock").clone();
        assert_eq!(
            seen,
            Some((
                "codex".to_string(),
                vec![
                    "exec".to_string(),
                    "--json".to_string(),
                    "--skip-git-repo-check".to_string(),
                    "--sandbox".to_string(),
                    "read-only".to_string(),
                    "-C".to_string(),
                    "/work".to_string(),
                    "-".to_string(),
                ]
            ))
        );
        // The default policy timeout is the long delegate ceiling.
        assert_eq!(
            *launcher.seen_timeout.lock().expect("timeout lock"),
            Some(DEFAULT_DELEGATE_TIMEOUT)
        );
    }

    #[test]
    fn claude_full_autonomy_argv_reaches_launcher() {
        let launcher = Arc::new(RecordingLauncher::new(""));
        let tools = delegate_box(FakeInner, launcher.clone(), true, None);
        tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "claude", "task": "fix it", "autonomy": "full" }),
                &CancelToken::never(),
            )
            .expect("enabled delegate runs");
        let seen = launcher
            .seen
            .lock()
            .expect("seen lock")
            .clone()
            .expect("seen");
        assert_eq!(seen.0, "claude");
        assert!(
            seen.1.iter().any(|a| a == "bypassPermissions"),
            "{:?}",
            seen.1
        );
    }

    #[test]
    fn custom_timeout_is_applied() {
        let launcher = Arc::new(RecordingLauncher::new(""));
        let tools = delegate_box(FakeInner, launcher.clone(), true, None);
        tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "x", "timeout_secs": 30 }),
                &CancelToken::never(),
            )
            .expect("runs");
        assert_eq!(
            *launcher.seen_timeout.lock().expect("timeout lock"),
            Some(Duration::from_secs(30))
        );
        // The unset default is the long delegate ceiling (600s), not the custom value.
        assert_eq!(DEFAULT_DELEGATE_TIMEOUT, Duration::from_secs(600));
    }

    #[test]
    fn streams_progress_and_parses_outcome() {
        let stream = concat!(
            r#"{"type":"item","item":{"type":"agent_message","text":"working"}}"#,
            "\n",
            r#"{"type":"item","item":{"type":"agent_message","text":"done"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":11,"output_tokens":7}}"#,
            "\n",
        );
        let captured: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_cap = Arc::clone(&captured);
        let sink: Arc<DelegateProgressSink> = Arc::new(move |agent: &str, text: &str| {
            sink_cap
                .lock()
                .expect("cap lock")
                .push((agent.to_string(), text.to_string()));
        });
        let tools = delegate_box(
            FakeInner,
            Arc::new(RecordingLauncher::new(stream)),
            true,
            Some(sink),
        );
        let out = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "do it" }),
                &CancelToken::never(),
            )
            .expect("runs");
        // Final outcome parsed from the stream.
        assert_eq!(out["agent"], "codex");
        assert_eq!(out["ok"], true);
        assert_eq!(out["result"], "done");
        assert_eq!(out["usage"]["input_tokens"], 11);
        // Each agent_message line streamed through the progress sink, tagged with
        // the agent name.
        assert_eq!(
            *captured.lock().expect("cap lock"),
            vec![
                ("codex".to_string(), "working".to_string()),
                ("codex".to_string(), "done".to_string()),
            ]
        );
    }

    #[test]
    fn unknown_agent_is_rejected_before_launch() {
        let launcher = Arc::new(RecordingLauncher::new(""));
        let tools = delegate_box(FakeInner, launcher.clone(), true, None);
        let err = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "rovo", "task": "x" }),
                &CancelToken::never(),
            )
            .expect_err("unknown agent rejected");
        assert!(err.to_string().contains("unknown delegate agent"));
        assert!(launcher.seen.lock().expect("seen lock").is_none());
    }

    #[test]
    fn cwd_escape_is_rejected_before_launch() {
        let launcher = Arc::new(RecordingLauncher::new(""));
        let tools = delegate_box(FakeInner, launcher.clone(), true, None);
        let err = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "x", "cwd": "../etc" }),
                &CancelToken::never(),
            )
            .expect_err("escape rejected");
        assert!(err.to_string().contains(".."));
        assert!(launcher.seen.lock().expect("seen lock").is_none());
    }

    #[test]
    fn empty_task_is_rejected() {
        let tools = delegate_box(FakeInner, Arc::new(RecordingLauncher::new("")), true, None);
        let err = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "   " }),
                &CancelToken::never(),
            )
            .expect_err("empty task rejected");
        assert!(err.to_string().contains("non-empty"));
    }

    #[test]
    fn refuse_launcher_surfaces_disabled_message() {
        // A refusing launcher (the default trust context) maps to the same
        // "disabled" guidance even when the tool is enabled.
        let tools = delegate_box(FakeInner, Arc::new(RefuseLauncher), true, None);
        let err = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "x" }),
                &CancelToken::never(),
            )
            .expect_err("refuse launcher errors");
        assert!(err.to_string().contains("--allow-delegate"));
    }

    #[test]
    fn cancelled_token_short_circuits_before_launch() {
        let launcher = Arc::new(RecordingLauncher::new(""));
        let tools = delegate_box(FakeInner, launcher.clone(), true, None);
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = tools
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "x" }),
                &cancel,
            )
            .expect_err("cancelled run must not launch");
        assert!(matches!(err, AgentError::Cancelled));
        assert!(launcher.seen.lock().expect("seen lock").is_none());
    }

    #[test]
    fn non_delegate_calls_delegate_to_inner() {
        let tools = delegate_box(FakeInner, Arc::new(RefuseLauncher), true, None);
        let out = tools
            .call("read_file", &json!({ "path": "x" }), &CancelToken::never())
            .expect("delegated to inner");
        assert_eq!(out["name"], "read_file");
    }

    #[test]
    fn policy_gate_outermost_blocks_delegate_before_launch() {
        use crate::policy::{Policy, ToolGate};
        // Compose the production shape — PolicyToolBox(DelegateAgentToolBox(inner))
        // — and assert the gate denies `delegate_agent` (exec-tier → Ask, no
        // approval) BEFORE the launcher is consulted. Pins north-star invariant 9
        // for delegation.
        let launcher = Arc::new(RecordingLauncher::new(""));
        let gated = ToolGate::deny(Policy::default()).wrap(delegate_box(
            FakeInner,
            launcher.clone(),
            true,
            None,
        ));
        let err = gated
            .call(
                DELEGATE_AGENT,
                &json!({ "agent": "codex", "task": "x" }),
                &CancelToken::never(),
            )
            .expect_err("gate must deny delegate_agent");
        assert!(err.to_string().contains("permission denied"));
        assert!(
            launcher.seen.lock().expect("seen lock").is_none(),
            "launcher must never run when the gate denies"
        );
    }
}
