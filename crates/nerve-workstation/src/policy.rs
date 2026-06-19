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
//! Decision model — a tool name is matched against an ordered rule list (first
//! match wins; each rule is an exact name or a `*` glob), falling back to a
//! default action:
//! - [`PolicyAction::Allow`] → delegate to the inner toolbox.
//! - [`PolicyAction::Deny`]  → return a readable tool-error; the tool never runs.
//! - [`PolicyAction::Ask`]   → consult an [`Approver`] (interactive CLI prompt;
//!   the daemon denies, since no human is on the socket).
//!
//! Defaults (security before openness): read-only / navigational tools are
//! allowed unprompted; everything else — mutating tools
//! (`edit`/`write`/`delete`/`move`/`ast_edit`) and third-party `mcp__*` tools —
//! falls through to `Ask`. `--allow-all` forces `Allow` for trusted batch runs.
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
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Tools that only read or navigate the immutable snapshot — safe to run without
/// prompting. Anything not listed (the mutating tools and any `mcp__*` plugin
/// tool) falls through to the policy's default action (`Ask`), so a newly added
/// tool of unknown nature is gated by default rather than silently allowed.
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
    "manage_selection",
    "workspace_context",
    "git",
];

/// Agent-internal state tools that write only to `.nerve/` (working / long-term
/// memory), never the codebase or the outside world — safe to run without prompting,
/// like the read-only tools. Auto-allowed so the outermost gate (invariant 9) does not
/// prompt for them.
const SAFE_AGENT_TOOLS: &[&str] = &["update_checkpoint", "remember"];

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
    default: PolicyAction,
    allow_all: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Policy {
    /// The built-in baseline: read-only tools `Allow`, everything else `Ask`.
    fn builtin() -> Self {
        Self {
            rules: builtin_allow_rules(),
            default: PolicyAction::Ask,
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

    /// Compose the built-in defaults with a trusted `global` layer and an
    /// untrusted `project` layer (first match wins). Precedence:
    /// 1. project `deny` rules — the only thing honored from the workspace, and a
    ///    deny only ever tightens, so it is safe to trust;
    /// 2. global rules — operator-controlled, may allow / deny / ask anything;
    /// 3. the built-in read-only allowlist.
    ///
    /// The fallback `default` comes from the global file (else `Ask`); the project
    /// file's `default` is deliberately ignored so an untrusted workspace cannot
    /// relax the gate.
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
        let mut default = PolicyAction::Ask;
        if let Some(global) = global {
            rules.extend(global.rules);
            if let Some(action) = global.default {
                default = action;
            }
        }
        rules.extend(builtin_allow_rules());
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

    /// Decide the action for `tool`: `allow_all` wins, else the first matching
    /// rule, else the default.
    pub(crate) fn decide(&self, tool: &str) -> PolicyAction {
        if self.allow_all {
            return PolicyAction::Allow;
        }
        self.rules
            .iter()
            .find(|rule| rule.matches(tool))
            .map_or(self.default, |rule| rule.action)
    }
}

/// Built-in allow rules: the read-only tools plus the agent-internal state tools
/// (`SAFE_AGENT_TOOLS`). Auto-allowed so making the permission gate the outermost
/// decorator (north-star invariant 9) does not start prompting for them.
fn builtin_allow_rules() -> Vec<PolicyRule> {
    READONLY_TOOLS
        .iter()
        .chain(SAFE_AGENT_TOOLS.iter())
        .map(|&tool| PolicyRule {
            tool: tool.to_string(),
            action: PolicyAction::Allow,
        })
        .collect()
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
    approver: Arc<dyn Approver>,
}

impl<T: ToolBox> PolicyToolBox<T> {
    fn new(inner: T, policy: Policy, approver: Arc<dyn Approver>) -> Self {
        Self {
            inner,
            policy,
            approver,
        }
    }
}

impl<T: ToolBox> ToolBox for PolicyToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        match self.policy.decide(name) {
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
    approver: Arc<dyn Approver>,
}

impl ToolGate {
    /// Interactive CLI gate: policy resolved from config plus `--allow-all`,
    /// approvals prompted on the terminal.
    pub(crate) fn cli(project_dir: Option<&Path>, allow_all: bool) -> Result<Self> {
        Ok(Self {
            policy: Policy::resolve(project_dir, allow_all)?,
            approver: Arc::new(CliApprover),
        })
    }

    /// Non-interactive daemon gate over an already-resolved policy: every `Ask`
    /// is denied (safe default; no human on the daemon socket).
    pub(crate) fn deny(policy: Policy) -> Self {
        Self {
            policy,
            approver: Arc::new(DenyApprover),
        }
    }

    /// Session gate over an already-resolved policy: `Ask` is delegated to the
    /// runtime-protocol approval round-trip.
    pub(crate) fn with_approver(policy: Policy, approver: Arc<dyn Approver>) -> Self {
        Self { policy, approver }
    }

    /// Wrap `inner` with this gate's policy and approver.
    pub(crate) fn wrap<T: ToolBox>(self, inner: T) -> PolicyToolBox<T> {
        PolicyToolBox::new(inner, self.policy, self.approver)
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

    // ---- policy decisions ----

    #[test]
    fn default_allows_readonly_asks_mutating_and_mcp() {
        let policy = Policy::default();
        for tool in [
            "read_file",
            "file_search",
            "git",
            "semantic_search",
            "get_repo_map",
            "update_checkpoint",
            "remember",
        ] {
            assert_eq!(policy.decide(tool), PolicyAction::Allow, "{tool}");
        }
        for tool in [
            "edit",
            "write",
            "delete",
            "move",
            "ast_edit",
            "spawn_agent",
            "run_command",
            "mcp__fs__write",
            "brand_new_tool",
        ] {
            assert_eq!(policy.decide(tool), PolicyAction::Ask, "{tool}");
        }
    }

    #[test]
    fn allow_all_overrides_everything() {
        let policy = Policy::from_file(None, true);
        assert_eq!(policy.decide("edit"), PolicyAction::Allow);
        assert_eq!(policy.decide("mcp__x__y"), PolicyAction::Allow);
        assert_eq!(policy.decide("read_file"), PolicyAction::Allow);
    }

    #[test]
    fn config_rules_override_builtin_allowlist() {
        // A project denies a normally-allowed read tool and allows mcp tools.
        let policy = Policy::from_file(
            Some(file(
                r#"{ "rules": [
                    { "tool": "git", "action": "deny" },
                    { "tool": "mcp__*", "action": "allow" }
                ] }"#,
            )),
            false,
        );
        assert_eq!(policy.decide("git"), PolicyAction::Deny); // user rule wins over built-in allow
        assert_eq!(policy.decide("mcp__fs__read"), PolicyAction::Allow); // loosened via glob
        assert_eq!(policy.decide("read_file"), PolicyAction::Allow); // built-in still applies
        assert_eq!(policy.decide("edit"), PolicyAction::Ask); // default unchanged
    }

    #[test]
    fn config_default_action_overrides_fallback() {
        let deny_default = Policy::from_file(Some(file(r#"{ "default": "deny" }"#)), false);
        assert_eq!(deny_default.decide("edit"), PolicyAction::Deny); // fallback is now deny
        assert_eq!(deny_default.decide("read_file"), PolicyAction::Allow); // built-in allow wins

        let allow_default = Policy::from_file(Some(file(r#"{ "default": "allow" }"#)), false);
        assert_eq!(allow_default.decide("edit"), PolicyAction::Allow);
        assert_eq!(allow_default.decide("mcp__x"), PolicyAction::Allow);
    }

    #[test]
    fn empty_config_equals_builtin() {
        let policy = Policy::from_file(Some(PolicyFile::default()), false);
        assert_eq!(policy.decide("read_file"), PolicyAction::Allow);
        assert_eq!(policy.decide("edit"), PolicyAction::Ask);
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
        assert_eq!(policy.decide("read_file"), PolicyAction::Deny); // deny honored (tighten)
        assert_eq!(policy.decide("edit"), PolicyAction::Ask); // allow rule dropped
        assert_eq!(policy.decide("mcp__x"), PolicyAction::Ask); // project default ignored
        assert_eq!(policy.decide("git"), PolicyAction::Allow); // built-in intact
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
        assert_eq!(policy.decide("edit"), PolicyAction::Deny); // global default wins
        assert_eq!(policy.decide("mcp__foo"), PolicyAction::Deny); // project allow dropped
        assert_eq!(policy.decide("read_file"), PolicyAction::Allow); // built-in read-only intact
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
        assert_eq!(policy.decide("read_file"), PolicyAction::Deny);
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

    fn gated(policy: Policy, approve: bool) -> PolicyToolBox<CountingToolBox> {
        PolicyToolBox::new(
            CountingToolBox::new(),
            policy,
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
