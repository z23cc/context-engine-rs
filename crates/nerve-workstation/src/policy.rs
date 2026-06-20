//! Permission / policy engine — authorize agent tool calls at the composition root.
//!
//! Architecture north star P4 (`docs/designs/architecture-north-star.md` §6.4
//! "lifecycle — hooks: …policy", §9 "security before openness"): before any
//! third-party plugin (MCP server, script-bearing skill) can be trusted, every
//! tool call an LLM makes must pass through an authorization decision.
//!
//! This module wraps the [`ToolBox`] seam with a [`PolicyToolBox`] in the binary
//! — the sole composition root. `nerve-agent` stays unaware of policy: the
//! orchestrator only ever sees `&dyn ToolBox`, so the gate is transparent and
//! the seam discipline holds (policy is a host concern, not an agent concern).
//!
//! Decision model — each tool call resolves to one action, evaluated in order
//! (see [`Policy::decide_with_mode`]):
//! 1. an explicit `policy.json` rule (first match of an exact name or `*` glob);
//! 2. else `--allow-all` → `Allow`;
//! 3. else the tool's static [`tool_tier`] vs. the session's [`ApprovalMode`] —
//!    a tool at or below the mode's `max_auto_tier` is auto-allowed;
//! 4. else an operator `default` (global config), else `Ask`.
//!
//! The resolved action is then enforced:
//! - [`PolicyAction::Allow`] → delegate to the inner toolbox.
//! - [`PolicyAction::Deny`]  → return a readable tool-error; the tool never runs.
//! - [`PolicyAction::Ask`]   → consult an [`Approver`] (interactive CLI prompt, the
//!   protocol approval round-trip for sessions, or deny on the bare daemon socket).
//!
//! Tier × mode (security before openness): tools are statically classified into
//! [`RiskTier`]s — `ReadOnly` (reads / navigation), `Edit` (file mutation:
//! `edit`/`write`/`delete`/`move`/`ast_edit`/`manage_selection`/`manage_workspaces`),
//! and `Exec` (`run_command`/`spawn_agent`/network + generation tools, and every
//! `mcp__*` or unknown tool, fail-safe). `AlwaysAsk` auto-allows only `ReadOnly`;
//! `Write` also auto-allows `Edit`; `Yolo` auto-allows everything. `--allow-all`
//! runs the CLI in `Yolo`; the bare daemon job runs `AlwaysAsk` + deny.
//!
//! Config (JSON, in the spirit of `--mcp-config` / `--provider-config` and the
//! P3 capabilities loader): a `policy.json` is read from the global config home
//! (`config_home()/policy.json`) and from the project (`<root>/.nerve/policy.json`),
//! both layered over the built-in defaults.
//!
//! Trust tiers (security before openness — the gate must not be silently disabled
//! by the workspace it is protecting against): the **global** file is
//! operator-controlled and fully authoritative — its `rules` (allow/deny/ask) and
//! optional `default` override the built-ins. The **project** file lives inside
//! the (possibly untrusted) workspace, so it is *tighten-only*: only its `deny`
//! rules are honored and its `default` is ignored. A project can harden its own
//! permissions but never relax them; loosening is the operator's prerogative
//! (global config or `--allow-all`).

use crate::workspace::ServeArgs;
use anyhow::{Context, Result};
use nerve_agent::{AgentError, AgentResult, ToolBox, ToolSpec};
use nerve_core::CancelToken;
use nerve_runtime::{ApprovalMode, RiskTier};
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Tools that only read or navigate the immutable snapshot — classified
/// [`RiskTier::ReadOnly`], so they are auto-approved under every [`ApprovalMode`]
/// (`ReadOnly` ≤ each mode's `max_auto_tier`). The mutating and exec tools are
/// classified one tier up (see [`EDIT_TOOLS`] / [`EXEC_TOOLS`] and [`tool_tier`]).
const READONLY_TOOLS: &[&str] = &[
    "file_search",
    "read_file",
    "get_code_structure",
    "get_repo_map",
    "goto_definition",
    "find_references",
    "call_hierarchy",
    "ast_search",
    "semantic_search",
    "build_context",
    "workspace_context",
    "git",
    // Discovery of delegatable external agents — a PATH probe, no spawn, no
    // mutation. The actual delegation tool (`delegate_agent`) is exec-tier.
    "list_agents",
];

/// Agent-internal state tools that write only to `.nerve/` (working / long-term
/// memory), never the codebase or the outside world — safe to run without prompting,
/// like the read-only tools. Auto-allowed so the outermost gate (invariant 9) does not
/// prompt for them.
const SAFE_AGENT_TOOLS: &[&str] = &["update_checkpoint", "remember"];

/// Tools that mutate workspace files (or the working selection) but never escape
/// the file sandbox — classified [`RiskTier::Edit`]. Auto-approved under the
/// `Write` and `Yolo` modes; prompt under `AlwaysAsk`.
const EDIT_TOOLS: &[&str] = &[
    "edit",
    "write",
    "delete",
    "move",
    "ast_edit",
    // Writes a persistent selection / workspace registration — a mutation of host
    // state, not a pure read, so it sits a tier above the read-only navigators.
    "manage_selection",
    "manage_workspaces",
];

/// Tools that run arbitrary commands, spawn agents, or reach the network /
/// generation backends — classified [`RiskTier::Exec`], the highest tier. Only
/// auto-approved under `Yolo`. Any `mcp__*` plugin tool and any unknown tool also
/// falls here (fail-safe), so a newly added capability is gated by default.
const EXEC_TOOLS: &[&str] = &[
    "run_command",
    "spawn_agent",
    "web_search",
    "x_search",
    "xai_x_search",
    "xai_responses",
    "xai_web_search",
    "xai_image_generate",
    "xai_video_generate",
    "xai_tts",
    "xai_transcribe",
    "openai_image_generate",
    // Delegates a coding task to an external agent CLI subprocess (DA-2) — the
    // highest-privilege surface (arbitrary edits/exec by another agent).
    "delegate_agent",
];

/// Static risk classification for a tool, mirroring oh-my-pi's per-tool tier
/// declarations. Fail-safe: any tool not explicitly listed as read-only or edit —
/// including every `mcp__*` plugin tool and any tool added later — is treated as
/// [`RiskTier::Exec`], the most-restricted tier, so an unclassified capability is
/// gated by default rather than silently auto-approved.
pub(crate) fn tool_tier(name: &str) -> RiskTier {
    if READONLY_TOOLS.contains(&name) || SAFE_AGENT_TOOLS.contains(&name) {
        RiskTier::ReadOnly
    } else if EDIT_TOOLS.contains(&name) {
        RiskTier::Edit
    } else if EXEC_TOOLS.contains(&name) {
        RiskTier::Exec
    } else {
        // Fail-safe residual: every `mcp__*` plugin tool and any tool not declared
        // above classify as Exec (the most restricted tier), so a newly added
        // capability is never silently auto-approved.
        RiskTier::Exec
    }
}

/// Rank a [`RiskTier`] least-to-most privileged so a mode's `max_auto_tier` can be
/// compared against a tool's tier with `<=` semantics (the enum is `Copy` but not
/// `Ord`, and keeping the ordering local avoids leaking it into the protocol type).
fn tier_rank(tier: RiskTier) -> u8 {
    match tier {
        RiskTier::ReadOnly => 0,
        RiskTier::Edit => 1,
        RiskTier::Exec => 2,
    }
}

/// Human-readable, truncated preview of a tool call for the approval prompt,
/// mirroring oh-my-pi's `formatApprovalDetails`: surface the one argument that
/// matters for a yes/no decision (the command, the path, the query, the prompt),
/// falling back to a compact JSON dump of the arguments.
pub(crate) fn format_preview(tool: &str, args: &Value) -> String {
    const MAX: usize = 500;
    let detail = match tool {
        "run_command" => command_preview(args),
        "delegate_agent" => delegate_preview(args),
        "edit" | "write" => string_field(args, "path"),
        "delete" => string_field(args, "path"),
        "move" => move_preview(args),
        "file_search" | "semantic_search" | "ast_search" | "build_context" | "web_search"
        | "x_search" | "xai_x_search" | "xai_web_search" => string_field(args, "query"),
        "xai_image_generate" | "xai_video_generate" | "openai_image_generate" => {
            string_field(args, "prompt")
        }
        _ => None,
    };
    let rendered = detail.unwrap_or_else(|| compact_json(args));
    truncate_chars(&rendered, MAX)
}

/// `run_command` preview: the `command` string, else a space-joined `argv`.
fn command_preview(args: &Value) -> Option<String> {
    if let Some(command) = args.get("command").and_then(Value::as_str) {
        return Some(command.to_string());
    }
    args.get("argv").and_then(Value::as_array).map(|argv| {
        argv.iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    })
}

/// `delegate_agent` preview: `<agent>: <task> (cwd <cwd>) [<autonomy>]`, so the
/// approval modal shows which external agent is being spawned, on what task, where,
/// and at what permission level. `cwd` defaults to the workspace root (shown as
/// `.`) and `autonomy` to `read_only` when omitted.
fn delegate_preview(args: &Value) -> Option<String> {
    let agent = args.get("agent").and_then(Value::as_str)?;
    let task = args.get("task").and_then(Value::as_str).unwrap_or("");
    let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let autonomy = args
        .get("autonomy")
        .and_then(Value::as_str)
        .unwrap_or("read_only");
    Some(format!("{agent}: {task} (cwd {cwd}) [{autonomy}]"))
}

/// `move` preview: `<from> -> <to>` when both are present, else whichever exists.
fn move_preview(args: &Value) -> Option<String> {
    let from = args.get("path").and_then(Value::as_str);
    let to = args.get("to").and_then(Value::as_str);
    match (from, to) {
        (Some(from), Some(to)) => Some(format!("{from} -> {to}")),
        (Some(from), None) => Some(from.to_string()),
        (None, Some(to)) => Some(to.to_string()),
        (None, None) => None,
    }
}

/// Read a string-valued `field` from `args`, if present and a string.
fn string_field(args: &Value, field: &str) -> Option<String> {
    args.get(field).and_then(Value::as_str).map(str::to_string)
}

/// Compact JSON of the arguments (the catch-all preview); `""` for null.
fn compact_json(args: &Value) -> String {
    if args.is_null() {
        String::new()
    } else {
        args.to_string()
    }
}

/// Truncate to at most `max` characters, appending an ellipsis when cut.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}\u{2026}")
    }
}

/// What to do with a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PolicyAction {
    /// Run the tool without prompting.
    Allow,
    /// Refuse the tool; the model receives a readable tool-error.
    Deny,
    /// Consult the [`Approver`] before running.
    Ask,
}

/// One policy rule: a tool-name pattern mapped to an action. The pattern is an
/// exact tool name or a glob where `*` matches any run of characters (every
/// other character is literal), e.g. `mcp__*` or `*edit`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PolicyRule {
    /// Tool-name pattern (exact or `*` glob).
    tool: String,
    /// Action applied on a match.
    action: PolicyAction,
}

impl PolicyRule {
    fn matches(&self, tool: &str) -> bool {
        glob_match(&self.tool, tool)
    }
}

/// On-disk shape of `policy.json`. Both fields are optional: an empty file is a
/// no-op layer over the built-in defaults.
#[derive(Debug, Clone, Default, Deserialize)]
struct PolicyFile {
    /// Fallback action for a tool matching no rule. Omitted ⇒ keep `Ask`.
    #[serde(default)]
    default: Option<PolicyAction>,
    /// Ordered rules; checked before the built-in read-only allowlist.
    #[serde(default)]
    rules: Vec<PolicyRule>,
}

/// A resolved authorization policy: an ordered rule list plus a fallback action.
/// `allow_all` short-circuits every decision to [`PolicyAction::Allow`].
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    rules: Vec<PolicyRule>,
    /// An *explicit* operator fallback from the global `policy.json` (`None` when
    /// unset). Applied only after the tier+mode auto-allow check, so a global
    /// `default: deny` still can't lock out the read-only tools the mode permits.
    default: Option<PolicyAction>,
    allow_all: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Policy {
    /// The built-in baseline: no explicit rules or fallback. Authorization is
    /// driven entirely by the tier×mode matrix at decision time.
    fn builtin() -> Self {
        Self {
            rules: Vec::new(),
            default: None,
            allow_all: false,
        }
    }

    /// Resolve the effective policy for an agent run: layer the operator's global
    /// `policy.json` and the project's (tighten-only) `policy.json` over the
    /// built-in defaults, then apply the `--allow-all` override. Fails closed on
    /// an unreadable/malformed config rather than silently degrading to permissive
    /// defaults.
    pub(crate) fn resolve(project_dir: Option<&Path>, allow_all: bool) -> Result<Self> {
        let global = load_policy_file(global_base())?;
        let project = load_policy_file(project_base(project_dir))?;
        Ok(Self::from_layers(global, project, allow_all))
    }

    /// Compose a trusted `global` layer and an untrusted `project` layer into an
    /// ordered rule list (first match wins). Precedence:
    /// 1. project `deny` rules — the only thing honored from the workspace, and a
    ///    deny only ever tightens, so it is safe to trust;
    /// 2. global rules — operator-controlled, may allow / deny / ask anything.
    ///
    /// The fallback `default` comes from the global file (`None` ⇒ rely on the
    /// tier×mode matrix); the project file's `default` is deliberately ignored so
    /// an untrusted workspace cannot relax the gate.
    fn from_layers(
        global: Option<PolicyFile>,
        project: Option<PolicyFile>,
        allow_all: bool,
    ) -> Self {
        let mut rules = Vec::new();
        if let Some(project) = project {
            rules.extend(
                project
                    .rules
                    .into_iter()
                    .filter(|rule| rule.action == PolicyAction::Deny),
            );
        }
        let mut default = None;
        if let Some(global) = global {
            rules.extend(global.rules);
            if let Some(action) = global.default {
                default = Some(action);
            }
        }
        Self {
            rules,
            default,
            allow_all,
        }
    }

    /// Single trusted-layer construction, used by tests to exercise the global
    /// (operator-authoritative) path without touching disk.
    #[cfg(test)]
    fn from_file(file: Option<PolicyFile>, allow_all: bool) -> Self {
        Self::from_layers(file, None, allow_all)
    }

    /// Decide the action for `tool` under approval `mode`. Resolution order:
    /// 1. an explicit `policy.json` rule match wins (project-deny / global rule);
    /// 2. else `allow_all` (CLI `--allow-all`) → `Allow`;
    /// 3. else the tool's static [`tool_tier`] ≤ the mode's `max_auto_tier` →
    ///    `Allow` (this subsumes the former hardcoded read-only allowlist: a
    ///    `ReadOnly` tool is ≤ every mode's max, so it is always auto-allowed);
    /// 4. else an explicit operator `default` from the global config, if any;
    /// 5. else `Ask`.
    pub(crate) fn decide_with_mode(&self, tool: &str, mode: ApprovalMode) -> PolicyAction {
        if let Some(rule) = self.rules.iter().find(|rule| rule.matches(tool)) {
            return rule.action;
        }
        if self.allow_all {
            return PolicyAction::Allow;
        }
        if tier_rank(tool_tier(tool)) <= tier_rank(mode.max_auto_tier()) {
            return PolicyAction::Allow;
        }
        self.default.unwrap_or(PolicyAction::Ask)
    }
}

/// Resolves an `Ask` decision to allow (`true`) or deny (`false`).
pub(crate) trait Approver: Send + Sync {
    /// Decide whether the pending `tool` call (with `args`) may run.
    fn approve(&self, tool: &str, args: &Value) -> bool;
}

/// Interactive approver for `nerve agent run`: prompts on stderr and reads a
/// yes/no from stdin. Anything other than `y`/`yes` — including EOF on a piped
/// or detached stdin — denies, so the gate fails closed without a terminal.
pub(crate) struct CliApprover;

impl Approver for CliApprover {
    fn approve(&self, tool: &str, args: &Value) -> bool {
        use std::io::Write as _;
        let mut stderr = std::io::stderr();
        let _ = write!(
            stderr,
            "\n\u{26a0}  permission: allow tool `{tool}`{}? [y/N] ",
            preview_args(args)
        );
        let _ = stderr.flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => false,
            Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        }
    }
}

/// Non-interactive approver: denies every `Ask`. The safe daemon default — no
/// human is on the daemon socket, and an interactive approval round-trip over
/// the runtime protocol belongs to the future Session layer, not P4.
pub(crate) struct DenyApprover;

impl Approver for DenyApprover {
    fn approve(&self, _tool: &str, _args: &Value) -> bool {
        false
    }
}

/// A short, single-line argument preview for the approval prompt.
fn preview_args(args: &Value) -> String {
    if args.is_null() {
        return String::new();
    }
    const MAX: usize = 120;
    let rendered = args.to_string();
    if rendered.chars().count() <= MAX {
        format!(" {rendered}")
    } else {
        let head: String = rendered.chars().take(MAX).collect();
        format!(" {head}\u{2026}")
    }
}

/// Wraps an inner [`ToolBox`] with policy enforcement. [`specs`](ToolBox::specs)
/// pass through unchanged — the model still sees every tool — while each
/// [`call`](ToolBox::call) is gated by the [`Policy`] and, for `Ask`, the
/// [`Approver`].
pub(crate) struct PolicyToolBox<T: ToolBox> {
    inner: T,
    policy: Policy,
    mode: ApprovalMode,
    approver: Arc<dyn Approver>,
}

impl<T: ToolBox> PolicyToolBox<T> {
    fn new(inner: T, policy: Policy, mode: ApprovalMode, approver: Arc<dyn Approver>) -> Self {
        Self {
            inner,
            policy,
            mode,
            approver,
        }
    }
}

impl<T: ToolBox> ToolBox for PolicyToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        match self.policy.decide_with_mode(name, self.mode) {
            PolicyAction::Allow => self.inner.call(name, args, cancel),
            PolicyAction::Deny => Err(denied(name, "policy")),
            PolicyAction::Ask if self.approver.approve(name, args) => {
                self.inner.call(name, args, cancel)
            }
            PolicyAction::Ask => Err(denied(name, "the operator")),
        }
    }
}

/// Build the tool-error returned to the model when a call is refused. It is a
/// readable message (surfaced as `error: …` in the transcript), not a panic, so
/// the model can choose a different, permitted action.
fn denied(tool: &str, by: &str) -> AgentError {
    AgentError::Tool(format!(
        "permission denied: tool `{tool}` was not allowed by {by}"
    ))
}

/// Policy + approver bundle handed to `run_agent` at the composition root. The
/// CLI builds an interactive gate; the daemon builds a deny-on-`Ask` gate.
#[derive(Clone)]
pub(crate) struct ToolGate {
    policy: Policy,
    /// Approval posture the tier×mode matrix is evaluated against. Set per gate at
    /// construction (CLI / daemon / session) and threaded into the `PolicyToolBox`.
    mode: ApprovalMode,
    approver: Arc<dyn Approver>,
}

impl ToolGate {
    /// Interactive CLI gate: policy resolved from config plus `--allow-all`,
    /// approvals prompted on the terminal. `--allow-all` runs in `Yolo` (every
    /// tier auto-approved); otherwise `AlwaysAsk` preserves the prior CLI posture
    /// of prompting for anything above read-only.
    pub(crate) fn cli(project_dir: Option<&Path>, allow_all: bool) -> Result<Self> {
        let mode = if allow_all {
            ApprovalMode::Yolo
        } else {
            ApprovalMode::AlwaysAsk
        };
        Ok(Self {
            policy: Policy::resolve(project_dir, allow_all)?,
            mode,
            approver: Arc::new(CliApprover),
        })
    }

    /// Non-interactive daemon gate over an already-resolved policy: every `Ask`
    /// is denied (safe default; no human on the daemon socket). Runs in
    /// `AlwaysAsk` so anything above read-only resolves to `Ask` → denied.
    pub(crate) fn deny(policy: Policy) -> Self {
        Self {
            policy,
            mode: ApprovalMode::AlwaysAsk,
            approver: Arc::new(DenyApprover),
        }
    }

    /// Session gate over an already-resolved policy: `Ask` is delegated to the
    /// runtime-protocol approval round-trip, evaluated against the session's
    /// current [`ApprovalMode`].
    pub(crate) fn with_approver(
        policy: Policy,
        mode: ApprovalMode,
        approver: Arc<dyn Approver>,
    ) -> Self {
        Self {
            policy,
            mode,
            approver,
        }
    }

    /// Wrap `inner` with this gate's policy, mode, and approver.
    pub(crate) fn wrap<T: ToolBox>(self, inner: T) -> PolicyToolBox<T> {
        PolicyToolBox::new(inner, self.policy, self.mode, self.approver)
    }
}

/// Resolve the daemon's policy from its serve arguments (the default workspace
/// root), with `allow_all` disabled — the daemon never auto-approves; it denies
/// on `Ask`. Resolved once at startup so jobs don't re-read config per call.
pub(crate) fn daemon_policy(args: &ServeArgs) -> Result<Policy> {
    Policy::resolve(args.roots.first().map(PathBuf::as_path), false)
}

/// Load and parse `<base>/policy.json` when `base` is present and the file
/// exists; `None` otherwise. Fails closed on an unreadable / malformed file.
fn load_policy_file(base: Option<PathBuf>) -> Result<Option<PolicyFile>> {
    let Some(base) = base else {
        return Ok(None);
    };
    let path = base.join("policy.json");
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read policy config: {}", path.display()))?;
    let parsed = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse policy config: {}", path.display()))?;
    Ok(Some(parsed))
}

/// Project discovery base: `<root>/.nerve` (the workspace-local, *untrusted*
/// config dir, mirroring the P3 capabilities chain). `None` without a root.
fn project_base(project_dir: Option<&Path>) -> Option<PathBuf> {
    project_dir.map(|root| root.join(".nerve"))
}

/// Global discovery base: the operator's config home (`$NERVE_HOME` /
/// `$XDG_CONFIG_HOME/nerve` / OS config dir). `None` when it cannot be resolved,
/// in which case the built-in defaults still apply.
fn global_base() -> Option<PathBuf> {
    nerve_agent::auth::config_home().ok()
}

/// Match `text` against `pattern`, where `*` matches any run of characters
/// (including empty) and every other byte matches literally. Tool names are
/// ASCII, so byte-wise comparison is correct; deterministic and allocation-free
/// (classic linear two-pointer with backtracking on the last `*`).
fn glob_match(pattern: &str, text: &str) -> bool {
    let (pattern, text) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if pattern.get(p) == Some(&b'*') {
            star = Some(p);
            mark = t;
            p += 1;
        } else if pattern.get(p) == Some(&text[t]) {
            p += 1;
            t += 1;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while pattern.get(p) == Some(&b'*') {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn file(json: &str) -> PolicyFile {
        serde_json::from_str(json).expect("policy file parse")
    }

    // ---- glob matcher ----

    #[test]
    fn glob_exact_and_wildcards() {
        assert!(glob_match("read_file", "read_file"));
        // An exact name must not behave like a prefix.
        assert!(!glob_match("git", "github"));
        assert!(glob_match("mcp__*", "mcp__fs__read"));
        assert!(!glob_match("mcp__*", "read_file"));
        assert!(glob_match("*edit", "ast_edit"));
        assert!(glob_match("*edit*", "ast_edit"));
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a*c", "abbbc"));
        assert!(!glob_match("a*c", "abbb"));
    }

    // ---- tool-tier classifier ----

    #[test]
    fn tool_tier_classifies_by_capability() {
        for tool in [
            "read_file",
            "file_search",
            "git",
            "semantic_search",
            "build_context",
            "workspace_context",
            "get_repo_map",
            "update_checkpoint",
            "remember",
            "list_agents",
        ] {
            assert_eq!(tool_tier(tool), RiskTier::ReadOnly, "{tool}");
        }
        for tool in [
            "edit",
            "write",
            "delete",
            "move",
            "ast_edit",
            "manage_selection",
            "manage_workspaces",
        ] {
            assert_eq!(tool_tier(tool), RiskTier::Edit, "{tool}");
        }
        for tool in [
            "run_command",
            "spawn_agent",
            "web_search",
            "x_search",
            "xai_x_search",
            "xai_responses",
            "xai_web_search",
            "xai_image_generate",
            "xai_video_generate",
            "openai_image_generate",
            "delegate_agent",
            // mcp + unknown fail safe to the top tier.
            "mcp__fs__write",
            "brand_new_tool",
        ] {
            assert_eq!(tool_tier(tool), RiskTier::Exec, "{tool}");
        }
    }

    // ---- tier × mode matrix (no config rules) ----

    /// Convenience: the built-in (no-config) policy resolved under `mode`.
    fn decide(tool: &str, mode: ApprovalMode) -> PolicyAction {
        Policy::default().decide_with_mode(tool, mode)
    }

    #[test]
    fn always_ask_allows_read_asks_edit_and_exec() {
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(decide("read_file", mode), PolicyAction::Allow);
        assert_eq!(decide("remember", mode), PolicyAction::Allow);
        assert_eq!(decide("edit", mode), PolicyAction::Ask);
        assert_eq!(decide("manage_selection", mode), PolicyAction::Ask);
        assert_eq!(decide("run_command", mode), PolicyAction::Ask);
        assert_eq!(decide("mcp__fs__write", mode), PolicyAction::Ask);
    }

    #[test]
    fn write_allows_read_and_edit_asks_exec() {
        let mode = ApprovalMode::Write;
        assert_eq!(decide("read_file", mode), PolicyAction::Allow);
        assert_eq!(decide("edit", mode), PolicyAction::Allow);
        assert_eq!(decide("manage_workspaces", mode), PolicyAction::Allow);
        assert_eq!(decide("run_command", mode), PolicyAction::Ask);
        assert_eq!(decide("spawn_agent", mode), PolicyAction::Ask);
        assert_eq!(decide("mcp__x__y", mode), PolicyAction::Ask);
    }

    #[test]
    fn yolo_allows_everything() {
        let mode = ApprovalMode::Yolo;
        for tool in ["read_file", "edit", "run_command", "spawn_agent", "mcp__x"] {
            assert_eq!(decide(tool, mode), PolicyAction::Allow, "{tool}");
        }
    }

    #[test]
    fn allow_all_overrides_everything() {
        // allow_all short-circuits every tier under any mode (use the strictest).
        let policy = Policy::from_file(None, true);
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(policy.decide_with_mode("edit", mode), PolicyAction::Allow);
        assert_eq!(
            policy.decide_with_mode("mcp__x__y", mode),
            PolicyAction::Allow
        );
        assert_eq!(
            policy.decide_with_mode("read_file", mode),
            PolicyAction::Allow
        );
    }

    #[test]
    fn config_rules_win_over_tier_and_mode() {
        // A project denies a normally-allowed read tool and allows mcp tools; the
        // explicit rule wins over the tier×mode decision in either direction.
        let policy = Policy::from_file(
            Some(file(
                r#"{ "rules": [
                    { "tool": "git", "action": "deny" },
                    { "tool": "mcp__*", "action": "allow" }
                ] }"#,
            )),
            false,
        );
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(policy.decide_with_mode("git", mode), PolicyAction::Deny); // rule wins over read-tier allow
        assert_eq!(
            policy.decide_with_mode("mcp__fs__read", mode),
            PolicyAction::Allow
        ); // rule loosens exec-tier
        assert_eq!(
            policy.decide_with_mode("read_file", mode),
            PolicyAction::Allow
        ); // tier allow intact
        assert_eq!(policy.decide_with_mode("edit", mode), PolicyAction::Ask); // unmatched edit-tier asks
    }

    #[test]
    fn global_default_applies_only_after_tier_allow() {
        // A global `default: deny` is the fallback for tools the mode does NOT
        // auto-allow; read-tier tools the mode permits are still allowed.
        let deny_default = Policy::from_file(Some(file(r#"{ "default": "deny" }"#)), false);
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(
            deny_default.decide_with_mode("edit", mode),
            PolicyAction::Deny
        ); // exceeds mode → fallback deny
        assert_eq!(
            deny_default.decide_with_mode("read_file", mode),
            PolicyAction::Allow
        ); // read-tier allowed

        let allow_default = Policy::from_file(Some(file(r#"{ "default": "allow" }"#)), false);
        assert_eq!(
            allow_default.decide_with_mode("edit", mode),
            PolicyAction::Allow
        );
        assert_eq!(
            allow_default.decide_with_mode("mcp__x", mode),
            PolicyAction::Allow
        );
    }

    #[test]
    fn empty_config_equals_builtin() {
        let policy = Policy::from_file(Some(PolicyFile::default()), false);
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(
            policy.decide_with_mode("read_file", mode),
            PolicyAction::Allow
        );
        assert_eq!(policy.decide_with_mode("edit", mode), PolicyAction::Ask);
    }

    #[test]
    fn action_parses_lowercase() {
        assert_eq!(
            file(r#"{"default":"ask"}"#).default,
            Some(PolicyAction::Ask)
        );
        let parsed = file(r#"{"rules":[{"tool":"x","action":"deny"}]}"#);
        assert_eq!(parsed.rules[0].action, PolicyAction::Deny);
        assert_eq!(parsed.rules[0].tool, "x");
    }

    // ---- approval preview ----

    #[test]
    fn format_preview_surfaces_the_salient_argument() {
        assert_eq!(
            format_preview("run_command", &serde_json::json!({ "command": "ls -la" })),
            "ls -la"
        );
        assert_eq!(
            format_preview(
                "run_command",
                &serde_json::json!({ "argv": ["git", "log"] })
            ),
            "git log"
        );
        assert_eq!(
            format_preview("edit", &serde_json::json!({ "path": "src/main.rs" })),
            "src/main.rs"
        );
        assert_eq!(
            format_preview("move", &serde_json::json!({ "path": "a.rs", "to": "b.rs" })),
            "a.rs -> b.rs"
        );
        assert_eq!(
            format_preview(
                "semantic_search",
                &serde_json::json!({ "query": "auth flow" })
            ),
            "auth flow"
        );
        assert_eq!(
            format_preview(
                "xai_image_generate",
                &serde_json::json!({ "prompt": "a cat" })
            ),
            "a cat"
        );
        // delegate_agent renders agent + task + cwd + autonomy, with defaults.
        assert_eq!(
            format_preview(
                "delegate_agent",
                &serde_json::json!({
                    "agent": "codex",
                    "task": "add a test",
                    "cwd": "crates/core",
                    "autonomy": "edit"
                })
            ),
            "codex: add a test (cwd crates/core) [edit]"
        );
        assert_eq!(
            format_preview(
                "delegate_agent",
                &serde_json::json!({ "agent": "claude", "task": "investigate" })
            ),
            "claude: investigate (cwd .) [read_only]"
        );
    }

    #[test]
    fn format_preview_falls_back_to_compact_json_and_truncates() {
        // Unknown tool with no salient field -> compact JSON dump.
        let preview = format_preview("mystery", &serde_json::json!({ "a": 1, "b": "x" }));
        assert!(preview.contains("\"a\":1"), "{preview}");
        // Null args render empty.
        assert_eq!(format_preview("mystery", &serde_json::Value::Null), "");
        // Overlong previews are truncated with an ellipsis.
        let long = "x".repeat(600);
        let preview = format_preview("run_command", &serde_json::json!({ "command": long }));
        assert_eq!(preview.chars().count(), 501); // 500 + ellipsis
        assert!(preview.ends_with('\u{2026}'));
    }

    // ---- config loading (disk) ----

    #[test]
    fn project_layer_is_tighten_only() {
        // A project file may deny (tighten) but its `allow` rules and `default`
        // are dropped — an untrusted workspace cannot relax its own gate.
        let project = Some(file(
            r#"{ "default": "allow",
                 "rules": [
                   { "tool": "read_file", "action": "deny" },
                   { "tool": "edit", "action": "allow" }
                 ] }"#,
        ));
        let policy = Policy::from_layers(None, project, false);
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(
            policy.decide_with_mode("read_file", mode),
            PolicyAction::Deny
        ); // deny honored (tighten)
        assert_eq!(policy.decide_with_mode("edit", mode), PolicyAction::Ask); // allow rule dropped
        assert_eq!(policy.decide_with_mode("mcp__x", mode), PolicyAction::Ask); // project default ignored
        assert_eq!(policy.decide_with_mode("git", mode), PolicyAction::Allow); // read-tier intact
    }

    #[test]
    fn project_cannot_loosen_a_global_lockdown() {
        // Operator locks down by default; the project tries to open it back up.
        let global = Some(file(r#"{ "default": "deny" }"#));
        let project = Some(file(
            r#"{ "rules": [
                   { "tool": "edit", "action": "allow" },
                   { "tool": "mcp__*", "action": "allow" }
                 ] }"#,
        ));
        let policy = Policy::from_layers(global, project, false);
        let mode = ApprovalMode::AlwaysAsk;
        assert_eq!(policy.decide_with_mode("edit", mode), PolicyAction::Deny); // global default wins
        assert_eq!(
            policy.decide_with_mode("mcp__foo", mode),
            PolicyAction::Deny
        ); // project allow dropped
        assert_eq!(
            policy.decide_with_mode("read_file", mode),
            PolicyAction::Allow
        ); // read-tier intact
    }

    #[test]
    fn resolve_reads_project_deny_rule_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".nerve")).expect("mkdir");
        std::fs::write(
            dir.path().join(".nerve/policy.json"),
            r#"{ "rules": [ { "tool": "read_file", "action": "deny" } ] }"#,
        )
        .expect("write policy");
        // A project deny rule is read from disk and applied; it precedes any
        // global allow / the built-in allowlist, so the result is independent of
        // the operator's (machine-specific) global config.
        let policy = Policy::resolve(Some(dir.path()), false).expect("resolve");
        assert_eq!(
            policy.decide_with_mode("read_file", ApprovalMode::AlwaysAsk),
            PolicyAction::Deny
        );
    }

    #[test]
    fn invalid_policy_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".nerve")).expect("mkdir");
        std::fs::write(dir.path().join(".nerve/policy.json"), "not json").expect("write");
        // Fail-closed: a malformed policy must not silently fall back to defaults.
        assert!(Policy::resolve(Some(dir.path()), false).is_err());
    }

    // ---- PolicyToolBox enforcement ----

    struct CountingToolBox {
        calls: AtomicUsize,
    }

    impl CountingToolBox {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl ToolBox for CountingToolBox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "read_file".into(),
                description: String::new(),
                input_schema: serde_json::json!({ "type": "object" }),
            }]
        }

        fn call(&self, name: &str, args: &Value, _cancel: &CancelToken) -> AgentResult<Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "ran": name, "args": args }))
        }
    }

    /// An approver with a fixed answer, standing in for the CLI prompt.
    struct FixedApprover(bool);

    impl Approver for FixedApprover {
        fn approve(&self, _tool: &str, _args: &Value) -> bool {
            self.0
        }
    }

    /// Gate the counting toolbox under the strictest mode (`AlwaysAsk`), so the
    /// tier×mode matrix exercises the `Ask` path for anything above read-only.
    fn gated(policy: Policy, approve: bool) -> PolicyToolBox<CountingToolBox> {
        PolicyToolBox::new(
            CountingToolBox::new(),
            policy,
            ApprovalMode::AlwaysAsk,
            Arc::new(FixedApprover(approve)),
        )
    }

    #[test]
    fn allow_delegates_to_inner() {
        let gate = gated(Policy::default(), false);
        let out = gate
            .call(
                "read_file",
                &serde_json::json!({ "path": "x" }),
                &CancelToken::never(),
            )
            .expect("allowed");
        assert_eq!(out["ran"], "read_file");
        assert_eq!(gate.inner.calls(), 1);
    }

    #[test]
    fn ask_denied_blocks_without_calling_inner() {
        // Default policy asks for `edit`; the approver says no -> blocked.
        let gate = gated(Policy::default(), false);
        let err = gate
            .call("edit", &serde_json::json!({}), &CancelToken::never())
            .expect_err("blocked");
        assert!(matches!(err, AgentError::Tool(_)));
        assert!(err.to_string().contains("permission denied"));
        assert_eq!(gate.inner.calls(), 0); // inner never ran
    }

    #[test]
    fn ask_approved_runs_inner() {
        let gate = gated(Policy::default(), true); // approver says yes
        let out = gate
            .call(
                "edit",
                &serde_json::json!({ "path": "x" }),
                &CancelToken::never(),
            )
            .expect("approved");
        assert_eq!(out["ran"], "edit");
        assert_eq!(gate.inner.calls(), 1);
    }

    #[test]
    fn deny_rule_blocks_even_with_approver() {
        // A `deny` rule never consults the approver, even one that would allow.
        let policy = Policy::from_file(
            Some(file(
                r#"{ "rules": [ { "tool": "edit", "action": "deny" } ] }"#,
            )),
            false,
        );
        let gate = gated(policy, true);
        let err = gate
            .call("edit", &serde_json::json!({}), &CancelToken::never())
            .expect_err("denied");
        assert!(err.to_string().contains("permission denied"));
        assert_eq!(gate.inner.calls(), 0);
    }

    #[test]
    fn allow_all_runs_mutating_without_approver() {
        // allow_all wins even though the approver would deny.
        let gate = gated(Policy::from_file(None, true), false);
        let out = gate
            .call("delete", &serde_json::json!({}), &CancelToken::never())
            .expect("allowed");
        assert_eq!(out["ran"], "delete");
        assert_eq!(gate.inner.calls(), 1);
    }

    #[test]
    fn specs_pass_through_unfiltered() {
        let gate = gated(Policy::default(), false);
        let specs = gate.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "read_file");
    }
}
