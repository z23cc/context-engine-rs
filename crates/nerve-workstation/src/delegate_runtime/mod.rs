//! DA-2: the external-agent delegate runtime.
//!
//! [`delegate.rs`](crate::delegate) ships the read-only catalog + `list_agents`
//! probe. This module is the *driving* half: it turns a
//! [`RuntimeCommand::DelegateStart`](nerve_runtime::RuntimeCommand) into a
//! [`CommandSpec`] for a headless agent CLI, runs it through the
//! [`SandboxLauncher`] streaming seam, surfaces progress, and parses the agent's
//! event stream into a structured [`DelegateOutcome`].
//!
//! ## Argv recipes (verified headless invocations; argv only, no shell)
//!
//! | agent  | autonomy  | flag mapping                                  |
//! |--------|-----------|-----------------------------------------------|
//! | codex  | read_only | `--sandbox read-only`                         |
//! | codex  | edit      | `--sandbox workspace-write`                   |
//! | codex  | full      | `--sandbox danger-full-access`                |
//! | claude | read_only | `--permission-mode plan`                      |
//! | claude | edit      | `--permission-mode acceptEdits`               |
//! | claude | full      | `--permission-mode bypassPermissions`         |
//! | gemini | read_only | `--approval-mode plan`                        |
//! | gemini | edit      | `--approval-mode auto_edit`                   |
//! | gemini | full      | `--approval-mode yolo`                        |
//!
//! codex and claude read the task from **stdin** (so a large task never hits an
//! argv length limit); gemini takes it as a `-p <task>` argument.
//!
//! ## Security posture
//!
//! The spawn uses [`SandboxPolicy::for_root`] (forced cwd, scrubbed env, capped
//! output) but with **`NetPolicy::Allow`** — a delegated agent must reach its LLM
//! API — and a **longer timeout** ([`DEFAULT_DELEGATE_TIMEOUT`]). The env scrub
//! still strips `*_TOKEN` / `*_KEY` / `*_SECRET`, so the child cannot inherit
//! nerve's credentials; it authenticates with its **own** on-disk login (e.g.
//! `claude login`, `codex login`). Autonomy defaults to the most restricted
//! ([`ReadOnly`](nerve_runtime::DelegateAutonomy::ReadOnly)); `edit`/`full` are
//! granted only when the caller passes the explicit `autonomy` arg.

mod tool_events;
pub(crate) use tool_events::{parse_tool_events, tool_event_to_agent_event};

use crate::sandbox::{CommandSpec, NetPolicy, SandboxPolicy};
use nerve_runtime::DelegateAutonomy;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Wall-clock ceiling for one delegated run. Far longer than the build/test
/// `run_command` default ([`crate::sandbox::DEFAULT_TIMEOUT`]) because a coding
/// agent runs a whole multi-turn task, not a single command.
pub(crate) const DEFAULT_DELEGATE_TIMEOUT: Duration = Duration::from_secs(600);

/// A delegate-runtime failure that is the *caller's* fault (bad agent name, a
/// `cwd` that escapes the workspace) — surfaced before any subprocess is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DelegateError {
    UnknownAgent(String),
    CwdEscape(String),
}

impl std::fmt::Display for DelegateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAgent(agent) => write!(
                f,
                "unknown delegate agent `{agent}` (known: codex, claude, gemini)"
            ),
            Self::CwdEscape(reason) => write!(f, "delegate cwd rejected: {reason}"),
        }
    }
}

/// The parsed result of a delegated run, independent of which CLI produced it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DelegateOutcome {
    /// Catalog agent name (codex / claude / gemini).
    pub(crate) agent: String,
    /// Whether the agent reported success (no error, non-failure subtype).
    pub(crate) ok: bool,
    /// The agent's final assistant message / result text.
    pub(crate) result: String,
    /// Process exit code (`None` when killed by signal — e.g. the timeout).
    pub(crate) exit_code: Option<i32>,
    /// Token usage, when the agent's stream reported it.
    pub(crate) usage: Option<DelegateUsage>,
    /// Reported run cost in USD, when the agent's stream carried it (claude).
    pub(crate) cost_usd: Option<f64>,
    /// Whether the wall-clock timeout killed the run.
    pub(crate) timed_out: bool,
}

/// Token usage parsed from a delegated agent's stream. Fields are optional per
/// agent: codex reports input/output, claude additionally reports cache tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DelegateUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: u64,
    pub(crate) cache_creation_tokens: u64,
}

impl DelegateOutcome {
    /// Render the outcome as the job-result JSON returned over the protocol.
    #[must_use]
    pub(crate) fn to_json(&self) -> Value {
        let usage = self.usage.map(|u| {
            json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "cache_read_tokens": u.cache_read_tokens,
                "cache_creation_tokens": u.cache_creation_tokens,
            })
        });
        json!({
            "agent": self.agent,
            "ok": self.ok,
            "result": self.result,
            "exit_code": self.exit_code,
            "usage": usage,
            "cost_usd": self.cost_usd,
            "timed_out": self.timed_out,
        })
    }
}

/// The on-stdin payload (the task) plus the argv to run, returned by
/// [`build_command`] so the caller can stream the task in while spawning the CLI.
pub(crate) struct DelegateInvocation {
    pub(crate) spec: CommandSpec,
    /// Text fed to the child's stdin (the task for codex/claude; empty for
    /// gemini, which takes the task as an argument).
    pub(crate) stdin: String,
}

/// Which CLI a catalog agent name maps to, selecting both the argv recipe and the
/// stream parser. Unknown names are rejected before any spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DelegateAgent {
    Codex,
    Claude,
    Gemini,
}

impl DelegateAgent {
    /// Resolve a catalog `agent` name (the `delegate.start` argument) to its CLI.
    pub(crate) fn from_name(name: &str) -> Result<Self, DelegateError> {
        match name {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "gemini" => Ok(Self::Gemini),
            other => Err(DelegateError::UnknownAgent(other.to_string())),
        }
    }

    /// The catalog agent name (the inverse of [`Self::from_name`]) — used to label
    /// progress events and result JSON for the live persistent sessions.
    #[must_use]
    pub(crate) fn catalog_name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }
}

/// Build the argv (and stdin payload) for a delegated run. `cwd` is the already
/// confined child working directory (see [`resolve_delegate_cwd`]); `model`
/// overrides the agent's default model when set.
///
/// DA-6: `mcp_disable_flags` are the pre-computed, sorted `-c
/// mcp_servers.<name>.enabled=false` pairs for the codex servers this run must skip
/// (see [`crate::delegate_codex_mcp`]). They apply to the **codex** one-shot recipe
/// only; claude/gemini ignore them. An empty slice leaves the argv unchanged.
pub(crate) fn build_command(
    agent: DelegateAgent,
    task: &str,
    cwd: &Path,
    autonomy: DelegateAutonomy,
    model: Option<&str>,
    mcp_disable_flags: &[String],
) -> DelegateInvocation {
    match agent {
        DelegateAgent::Codex => build_codex(task, cwd, autonomy, model, mcp_disable_flags),
        DelegateAgent::Claude => build_claude(task, cwd, autonomy, model),
        DelegateAgent::Gemini => build_gemini(task, cwd, autonomy, model),
    }
}

/// `codex exec --json --skip-git-repo-check [-c mcp_servers.<n>.enabled=false …]
/// --sandbox <S> -C <cwd> [-m <model>] -` with the task on stdin (the trailing `-`
/// makes codex read the prompt from stdin). Emits JSONL events on stdout. The DA-6
/// MCP-disable `-c` pairs (if any) are inserted before `--sandbox` so codex applies
/// them at boot, in the order given (callers pass an already-sorted set).
fn build_codex(
    task: &str,
    cwd: &Path,
    autonomy: DelegateAutonomy,
    model: Option<&str>,
    mcp_disable_flags: &[String],
) -> DelegateInvocation {
    let sandbox = match autonomy {
        DelegateAutonomy::ReadOnly => "read-only",
        DelegateAutonomy::Edit => "workspace-write",
        DelegateAutonomy::Full => "danger-full-access",
    };
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--skip-git-repo-check".to_string(),
    ];
    args.extend(mcp_disable_flags.iter().cloned());
    args.push("--sandbox".to_string());
    args.push(sandbox.to_string());
    args.push("-C".to_string());
    args.push(cwd.display().to_string());
    if let Some(model) = model {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    args.push("-".to_string());
    DelegateInvocation {
        spec: CommandSpec {
            command: "codex".to_string(),
            args,
        },
        stdin: task.to_string(),
    }
}

/// `claude -p --output-format stream-json --verbose [--model <m>]
/// --permission-mode <P> --add-dir <cwd>` with the task on stdin. Emits
/// newline-delimited stream-json events on stdout.
fn build_claude(
    task: &str,
    cwd: &Path,
    autonomy: DelegateAutonomy,
    model: Option<&str>,
) -> DelegateInvocation {
    let permission_mode = match autonomy {
        DelegateAutonomy::ReadOnly => "plan",
        DelegateAutonomy::Edit => "acceptEdits",
        DelegateAutonomy::Full => "bypassPermissions",
    };
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
    ];
    if let Some(model) = model {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.push("--permission-mode".to_string());
    args.push(permission_mode.to_string());
    args.push("--add-dir".to_string());
    args.push(cwd.display().to_string());
    DelegateInvocation {
        spec: CommandSpec {
            command: "claude".to_string(),
            args,
        },
        stdin: task.to_string(),
    }
}

/// `gemini -p <task> -o stream-json --approval-mode <A> --skip-trust [-m <m>]`.
/// gemini takes the task as an argument (not stdin), so stdin is empty.
///
/// NOTE (honest partial): the gemini recipe is built from its documented flags
/// but has **not** been exercised against a live `gemini` CLI (no GEMINI_API_KEY
/// in this environment). The codex/claude paths are the verified ones.
fn build_gemini(
    task: &str,
    cwd: &Path,
    autonomy: DelegateAutonomy,
    model: Option<&str>,
) -> DelegateInvocation {
    let approval_mode = match autonomy {
        DelegateAutonomy::ReadOnly => "plan",
        DelegateAutonomy::Edit => "auto_edit",
        DelegateAutonomy::Full => "yolo",
    };
    let mut args = vec![
        "-p".to_string(),
        task.to_string(),
        "-o".to_string(),
        "stream-json".to_string(),
        "--approval-mode".to_string(),
        approval_mode.to_string(),
        "--skip-trust".to_string(),
    ];
    if let Some(model) = model {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    // gemini has no cwd flag in this recipe; the launcher forces the cwd via the
    // SandboxPolicy, so confinement is handled there rather than on the argv.
    let _ = cwd;
    DelegateInvocation {
        spec: CommandSpec {
            command: "gemini".to_string(),
            args,
        },
        stdin: String::new(),
    }
}

/// The containment policy for a delegated spawn: [`SandboxPolicy::for_root`] with
/// the network **allowed** (the agent calls its LLM API) and the longer delegate
/// timeout. Env secrets are still scrubbed by the launcher.
pub(crate) fn delegate_policy(cwd: &Path) -> SandboxPolicy {
    let mut policy = SandboxPolicy::for_root(Some(cwd));
    policy.net = NetPolicy::Allow;
    policy.timeout = DEFAULT_DELEGATE_TIMEOUT;
    // A GUI-launched daemon inherits a minimal launchd PATH; hand the delegated
    // agent a repaired PATH (see [`crate::agent_path`]) so it can find its own
    // subtools (git, language toolchains) under that minimal environment. Pushed onto
    // (not replacing) the determinism pins `for_root` set, so locale/TZ stay fixed.
    policy.env_overrides.push((
        "PATH".to_string(),
        crate::agent_path::child_path()
            .to_string_lossy()
            .into_owned(),
    ));
    policy
}

/// Resolve a caller-supplied delegate `cwd` against the workspace `root`, rejecting
/// anything that escapes it. Mirrors `exec_tool::resolve_cwd`: no `..` traversal,
/// no absolute path outside the root. With no `cwd`, the run uses `root`.
pub(crate) fn resolve_delegate_cwd(
    root: &Path,
    relative: Option<&str>,
) -> Result<PathBuf, DelegateError> {
    let Some(relative) = relative else {
        return Ok(root.to_path_buf());
    };
    let candidate = Path::new(relative);
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(DelegateError::CwdEscape(format!(
            "`{relative}` must not contain `..`"
        )));
    }
    if candidate.is_absolute() {
        if candidate.starts_with(root) {
            return Ok(candidate.to_path_buf());
        }
        return Err(DelegateError::CwdEscape(format!(
            "`{relative}` is outside the workspace root"
        )));
    }
    Ok(root.join(candidate))
}

/// Streaming parser state for one delegated run: accumulates the final result and
/// usage as stream lines arrive, and renders the human-meaningful text for a
/// `DelegateProgress` event from each line (filtering envelope noise).
pub(crate) struct DelegateParser {
    agent: DelegateAgent,
    final_result: String,
    ok: bool,
    usage: Option<DelegateUsage>,
    cost_usd: Option<f64>,
}

impl DelegateParser {
    #[must_use]
    pub(crate) fn new(agent: DelegateAgent) -> Self {
        Self {
            agent,
            final_result: String::new(),
            // Default to success; only an explicit failure subtype/flag flips it.
            ok: true,
            usage: None,
            cost_usd: None,
        }
    }

    /// Ingest one stdout line, updating the accumulated result/usage. Returns the
    /// human-meaningful progress text to emit (assistant/message chunks), or
    /// `None` for envelope/noise lines that should not be streamed verbatim.
    pub(crate) fn ingest(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            // Non-JSON line (e.g. a CLI banner) — surface it raw as progress.
            return Some(line.to_string());
        };
        match self.agent {
            DelegateAgent::Codex => self.ingest_codex(&value),
            DelegateAgent::Claude => self.ingest_claude(&value),
            DelegateAgent::Gemini => self.ingest_gemini(&value),
        }
    }

    /// codex JSONL: the final answer is the last `item` whose `type` is
    /// `agent_message` (its `.text`); usage rides on `turn.completed.usage`.
    fn ingest_codex(&mut self, value: &Value) -> Option<String> {
        let kind = value.get("type").and_then(Value::as_str);
        match kind {
            Some("item") | Some("item.completed") => {
                let item = value.get("item").unwrap_or(value);
                if item.get("type").and_then(Value::as_str) == Some("agent_message")
                    && let Some(text) = item.get("text").and_then(Value::as_str)
                {
                    self.final_result = text.to_string();
                    return Some(text.to_string());
                }
                None
            }
            Some("turn.completed") => {
                if let Some(usage) = value.get("usage") {
                    self.usage = Some(parse_codex_usage(usage));
                }
                None
            }
            _ => None,
        }
    }

    /// claude stream-json: the final answer is the `result` object's `.result`;
    /// success is `subtype == "success" && !is_error`; `total_cost_usd` and
    /// `.usage` carry cost/tokens. Streamed `assistant` messages surface as
    /// progress text.
    fn ingest_claude(&mut self, value: &Value) -> Option<String> {
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") => claude_assistant_text(value),
            Some("result") => {
                self.ok = value.get("subtype").and_then(Value::as_str) == Some("success")
                    && !value
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                if let Some(text) = value.get("result").and_then(Value::as_str) {
                    self.final_result = text.to_string();
                }
                self.cost_usd = value.get("total_cost_usd").and_then(Value::as_f64);
                if let Some(usage) = value.get("usage") {
                    self.usage = Some(parse_claude_usage(usage));
                }
                None
            }
            _ => None,
        }
    }

    /// gemini stream-json (UNVERIFIED — see [`build_gemini`]): best-effort mirror
    /// of the claude shape (assistant content chunks; a final `result`).
    fn ingest_gemini(&mut self, value: &Value) -> Option<String> {
        match value.get("type").and_then(Value::as_str) {
            Some("assistant") | Some("content") => {
                let text = value
                    .get("text")
                    .or_else(|| value.get("content"))
                    .and_then(Value::as_str)?;
                Some(text.to_string())
            }
            Some("result") => {
                if let Some(text) = value.get("result").and_then(Value::as_str) {
                    self.final_result = text.to_string();
                }
                // TODO(gemini-verified): once the live stream-json shape is confirmed,
                // flip self.ok on its error marker here. Today gemini success is
                // exit-code-driven only (self.ok stays true); see `gemini_outcome_*`.
                None
            }
            _ => None,
        }
    }

    /// Finish parsing, combining the accumulated stream state with the process
    /// result into the structured [`DelegateOutcome`].
    #[must_use]
    pub(crate) fn finish(
        self,
        agent_name: &str,
        exit_code: Option<i32>,
        timed_out: bool,
    ) -> DelegateOutcome {
        // A timeout or non-zero exit is a failure regardless of what the stream
        // claimed (a partial stream may not carry a failure marker).
        let process_ok = !timed_out && exit_code == Some(0);
        DelegateOutcome {
            agent: agent_name.to_string(),
            ok: self.ok && process_ok,
            result: self.final_result,
            exit_code,
            usage: self.usage,
            cost_usd: self.cost_usd,
            timed_out,
        }
    }
}

/// Concatenate the text of a claude `assistant` message's content blocks.
fn claude_assistant_text(value: &Value) -> Option<String> {
    let content = value.get("message")?.get("content")?.as_array()?;
    let text: String = content
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    (!text.is_empty()).then_some(text)
}

fn parse_codex_usage(usage: &Value) -> DelegateUsage {
    DelegateUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: usage
            .get("cached_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation_tokens: 0,
    }
}

fn parse_claude_usage(usage: &Value) -> DelegateUsage {
    DelegateUsage {
        input_tokens: usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        output_tokens: usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: usage
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        cache_creation_tokens: usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_agent_is_rejected() {
        let err = DelegateAgent::from_name("rovo").expect_err("unknown agent");
        assert_eq!(err, DelegateError::UnknownAgent("rovo".to_string()));
        assert!(err.to_string().contains("codex, claude, gemini"));
    }

    #[test]
    fn known_agents_resolve() {
        assert_eq!(
            DelegateAgent::from_name("codex").unwrap(),
            DelegateAgent::Codex
        );
        assert_eq!(
            DelegateAgent::from_name("claude").unwrap(),
            DelegateAgent::Claude
        );
        assert_eq!(
            DelegateAgent::from_name("gemini").unwrap(),
            DelegateAgent::Gemini
        );
    }

    #[test]
    fn codex_argv_read_only_vs_workspace_write() {
        let cwd = Path::new("/work");
        let ro = build_command(
            DelegateAgent::Codex,
            "do it",
            cwd,
            DelegateAutonomy::ReadOnly,
            None,
            &[],
        );
        assert_eq!(ro.spec.command, "codex");
        assert_eq!(
            ro.spec.args,
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "read-only",
                "-C",
                "/work",
                "-",
            ]
        );
        assert_eq!(ro.stdin, "do it");

        let edit = build_command(
            DelegateAgent::Codex,
            "do it",
            cwd,
            DelegateAutonomy::Edit,
            Some("o3"),
            &[],
        );
        assert_eq!(
            edit.spec.args,
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--sandbox",
                "workspace-write",
                "-C",
                "/work",
                "-m",
                "o3",
                "-",
            ]
        );
    }

    #[test]
    fn codex_full_autonomy_maps_to_danger_full_access() {
        let inv = build_command(
            DelegateAgent::Codex,
            "t",
            Path::new("/w"),
            DelegateAutonomy::Full,
            None,
            &[],
        );
        assert!(inv.spec.args.iter().any(|a| a == "danger-full-access"));
    }

    #[test]
    fn codex_one_shot_argv_inserts_mcp_disable_flags_before_sandbox() {
        // DA-6: disabled {b, c} -> their `-c …=false` pairs land between
        // --skip-git-repo-check and --sandbox, in the order given (sorted by the
        // caller). The allowed server "a" is never disabled.
        let flags = crate::delegate_codex_mcp::disable_flags(&["b".to_string(), "c".to_string()]);
        let inv = build_command(
            DelegateAgent::Codex,
            "do it",
            Path::new("/work"),
            DelegateAutonomy::ReadOnly,
            None,
            &flags,
        );
        assert_eq!(
            inv.spec.args,
            vec![
                "exec",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "mcp_servers.b.enabled=false",
                "-c",
                "mcp_servers.c.enabled=false",
                "--sandbox",
                "read-only",
                "-C",
                "/work",
                "-",
            ]
        );
        assert!(
            !inv.spec
                .args
                .iter()
                .any(|a| a == "mcp_servers.a.enabled=false"),
            "{:?}",
            inv.spec.args
        );
    }

    #[test]
    fn mcp_disable_flags_only_apply_to_codex() {
        // claude/gemini ignore the codex MCP-disable flags entirely.
        let flags = crate::delegate_codex_mcp::disable_flags(&["b".to_string()]);
        let claude = build_command(
            DelegateAgent::Claude,
            "t",
            Path::new("/w"),
            DelegateAutonomy::ReadOnly,
            None,
            &flags,
        );
        assert!(
            !claude.spec.args.iter().any(|a| a.contains("mcp_servers")),
            "{:?}",
            claude.spec.args
        );
        let gemini = build_command(
            DelegateAgent::Gemini,
            "t",
            Path::new("/w"),
            DelegateAutonomy::ReadOnly,
            None,
            &flags,
        );
        assert!(
            !gemini.spec.args.iter().any(|a| a.contains("mcp_servers")),
            "{:?}",
            gemini.spec.args
        );
    }

    #[test]
    fn claude_argv_plan_vs_accept_edits() {
        let cwd = Path::new("/work");
        let plan = build_command(
            DelegateAgent::Claude,
            "investigate",
            cwd,
            DelegateAutonomy::ReadOnly,
            None,
            &[],
        );
        assert_eq!(plan.spec.command, "claude");
        assert_eq!(
            plan.spec.args,
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--permission-mode",
                "plan",
                "--add-dir",
                "/work",
            ]
        );
        assert_eq!(plan.stdin, "investigate");

        let edit = build_command(
            DelegateAgent::Claude,
            "fix",
            cwd,
            DelegateAutonomy::Edit,
            Some("claude-sonnet-4-6"),
            &[],
        );
        assert_eq!(
            edit.spec.args,
            vec![
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "claude-sonnet-4-6",
                "--permission-mode",
                "acceptEdits",
                "--add-dir",
                "/work",
            ]
        );
    }

    #[test]
    fn claude_full_autonomy_maps_to_bypass_permissions() {
        let inv = build_command(
            DelegateAgent::Claude,
            "t",
            Path::new("/w"),
            DelegateAutonomy::Full,
            None,
            &[],
        );
        assert!(inv.spec.args.iter().any(|a| a == "bypassPermissions"));
    }

    #[test]
    fn gemini_argv_passes_task_as_arg_and_maps_modes() {
        let inv = build_command(
            DelegateAgent::Gemini,
            "summarize",
            Path::new("/w"),
            DelegateAutonomy::Full,
            Some("gemini-2.5-pro"),
            &[],
        );
        assert_eq!(inv.spec.command, "gemini");
        assert_eq!(
            inv.spec.args,
            vec![
                "-p",
                "summarize",
                "-o",
                "stream-json",
                "--approval-mode",
                "yolo",
                "--skip-trust",
                "-m",
                "gemini-2.5-pro",
            ]
        );
        // gemini reads the task from argv, not stdin.
        assert_eq!(inv.stdin, "");
    }

    #[test]
    fn cwd_defaults_to_root_and_rejects_escape() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("work");
        let relative = root.join("crates/core");
        assert_eq!(resolve_delegate_cwd(&root, None).unwrap(), root);
        assert_eq!(
            resolve_delegate_cwd(&root, Some("crates/core")).unwrap(),
            relative
        );
        let err = resolve_delegate_cwd(&root, Some("../etc")).expect_err("escape rejected");
        assert!(matches!(err, DelegateError::CwdEscape(_)));
        let outside_path = temp.path().join("outside");
        let outside = resolve_delegate_cwd(&root, outside_path.to_str())
            .expect_err("absolute outside rejected");
        assert!(matches!(outside, DelegateError::CwdEscape(_)));
    }

    #[test]
    fn delegate_policy_allows_net_and_extends_timeout() {
        let policy = delegate_policy(Path::new("/work"));
        assert_eq!(policy.net, NetPolicy::Allow);
        assert_eq!(policy.timeout, DEFAULT_DELEGATE_TIMEOUT);
        assert_eq!(policy.cwd, PathBuf::from("/work"));
    }

    #[test]
    fn codex_parser_extracts_final_message_and_usage() {
        // Canned codex JSONL: a couple of items, a final agent_message, a
        // turn.completed carrying usage.
        let lines = [
            r#"{"type":"item","item":{"type":"reasoning","text":"thinking"}}"#,
            r#"{"type":"item","item":{"type":"agent_message","text":"first"}}"#,
            r#"{"type":"item","item":{"type":"agent_message","text":"the answer"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":120,"output_tokens":45,"cached_input_tokens":10}}"#,
        ];
        let mut parser = DelegateParser::new(DelegateAgent::Codex);
        let mut progress = Vec::new();
        for line in lines {
            if let Some(text) = parser.ingest(line) {
                progress.push(text);
            }
        }
        // Both agent_message items streamed as progress; the last is the final.
        assert_eq!(progress, vec!["first", "the answer"]);
        let outcome = parser.finish("codex", Some(0), false);
        assert!(outcome.ok);
        assert_eq!(outcome.result, "the answer");
        assert_eq!(
            outcome.usage,
            Some(DelegateUsage {
                input_tokens: 120,
                output_tokens: 45,
                cache_read_tokens: 10,
                cache_creation_tokens: 0,
            })
        );
    }

    #[test]
    fn claude_parser_extracts_result_success_cost_and_usage() {
        let lines = [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"all done","total_cost_usd":0.0123,"usage":{"input_tokens":200,"output_tokens":80,"cache_read_input_tokens":50,"cache_creation_input_tokens":12}}"#,
        ];
        let mut parser = DelegateParser::new(DelegateAgent::Claude);
        let mut progress = Vec::new();
        for line in lines {
            if let Some(text) = parser.ingest(line) {
                progress.push(text);
            }
        }
        assert_eq!(progress, vec!["working on it"]);
        let outcome = parser.finish("claude", Some(0), false);
        assert!(outcome.ok);
        assert_eq!(outcome.result, "all done");
        assert_eq!(outcome.cost_usd, Some(0.0123));
        assert_eq!(
            outcome.usage,
            Some(DelegateUsage {
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: 50,
                cache_creation_tokens: 12,
            })
        );
    }

    #[test]
    fn claude_parser_flags_error_result_as_not_ok() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#;
        let mut parser = DelegateParser::new(DelegateAgent::Claude);
        assert!(parser.ingest(line).is_none());
        let outcome = parser.finish("claude", Some(1), false);
        assert!(!outcome.ok);
        assert_eq!(outcome.result, "boom");
    }

    #[test]
    fn gemini_outcome_is_exit_code_driven_today() {
        // Finding 8: the gemini stream parser never flips `ok` (no verified error
        // marker yet), so its success is purely exit-code-driven. Pin that contract
        // so a future verified-shape change is a deliberate, reviewed flip rather
        // than a silent regression. Do NOT guess gemini's error field here.
        let result_line = r#"{"type":"result","result":"all done"}"#;

        // exit 0 -> ok, even though the stream carries no success marker.
        let mut parser = DelegateParser::new(DelegateAgent::Gemini);
        assert!(parser.ingest(result_line).is_none());
        let ok = parser.finish("gemini", Some(0), false);
        assert!(ok.ok, "gemini is ok on a clean exit (exit-code-driven)");
        assert_eq!(ok.result, "all done");

        // non-zero exit -> not ok (the only failure signal gemini has today).
        let mut parser = DelegateParser::new(DelegateAgent::Gemini);
        parser.ingest(result_line);
        let failed = parser.finish("gemini", Some(1), false);
        assert!(!failed.ok, "a non-zero exit is never ok");

        // a timeout is likewise never ok.
        let mut parser = DelegateParser::new(DelegateAgent::Gemini);
        parser.ingest(result_line);
        let timed = parser.finish("gemini", None, true);
        assert!(!timed.ok, "a timed-out run is never ok");
        assert!(timed.timed_out);
    }

    #[test]
    fn timeout_or_nonzero_exit_forces_failure_even_on_success_stream() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#;
        let mut parser = DelegateParser::new(DelegateAgent::Claude);
        parser.ingest(line);
        let timed = parser.finish("claude", None, true);
        assert!(!timed.ok, "a timed-out run is never ok");
        assert!(timed.timed_out);

        let mut parser = DelegateParser::new(DelegateAgent::Claude);
        parser.ingest(line);
        let nonzero = parser.finish("claude", Some(2), false);
        assert!(!nonzero.ok, "a non-zero exit is never ok");
    }

    #[test]
    fn non_json_line_is_surfaced_as_raw_progress() {
        let mut parser = DelegateParser::new(DelegateAgent::Codex);
        assert_eq!(
            parser.ingest("a banner line"),
            Some("a banner line".to_string())
        );
        assert_eq!(parser.ingest("   "), None);
    }

    #[test]
    fn outcome_json_shape_round_trips() {
        let outcome = DelegateOutcome {
            agent: "codex".into(),
            ok: true,
            result: "done".into(),
            exit_code: Some(0),
            usage: Some(DelegateUsage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_creation_tokens: 4,
            }),
            cost_usd: Some(0.5),
            timed_out: false,
        };
        let json = outcome.to_json();
        assert_eq!(json["agent"], "codex");
        assert_eq!(json["ok"], true);
        assert_eq!(json["result"], "done");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["usage"]["input_tokens"], 1);
        assert_eq!(json["usage"]["cache_creation_tokens"], 4);
        assert_eq!(json["cost_usd"], 0.5);
        assert_eq!(json["timed_out"], false);
    }
}
