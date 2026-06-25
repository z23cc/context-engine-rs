//! Static per-tool risk classification + human-readable approval previews.
//!
//! Split out of the policy engine so the tier tables and the preview renderers
//! (the data + formatting half of P4) stay one responsibility, separate from the
//! [`Policy`](super::Policy) decision/enforcement machinery. `tool_tier` and
//! `format_preview` are re-exported by the parent module under their original
//! `crate::policy::*` paths.

use nerve_runtime::RiskTier;
use serde_json::Value;

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
pub(super) fn tier_rank(tier: RiskTier) -> u8 {
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
        "file_search" | "ast_search" | "build_context" | "web_search" | "x_search"
        | "xai_x_search" | "xai_web_search" => string_field(args, "query"),
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
