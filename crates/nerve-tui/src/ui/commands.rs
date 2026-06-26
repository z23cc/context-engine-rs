//! Pure slash-command helpers: parsing, the autocomplete palette, approval-mode
//! spellings, and model-list formatting.

use nerve_runtime::{ApprovalMode, DelegateRole};
use serde_json::Value;

use crate::app::state::Tone;

/// A parsed `/command rest...` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub cmd: String,
    pub rest: String,
}

/// Parse a `/command args...` line; `None` for ordinary messages. Ports
/// `parseCommand`: the command is lowercased, the rest is trimmed.
#[must_use]
pub fn parse_command(line: &str) -> Option<SlashCommand> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('/')?;
    match body.find(' ') {
        None => Some(SlashCommand {
            cmd: body.to_ascii_lowercase(),
            rest: String::new(),
        }),
        Some(space) => Some(SlashCommand {
            cmd: body[..space].to_ascii_lowercase(),
            rest: body[space + 1..].trim().to_string(),
        }),
    }
}

/// Parse a friendly approval-mode spelling into [`ApprovalMode`]. Accepts
/// "always-ask"/"always_ask"/"ask" → AlwaysAsk, "write", "yolo"; `None` for
/// anything unrecognized. Ports `parseApprovalMode`.
#[must_use]
pub fn parse_approval_mode(value: &str) -> Option<ApprovalMode> {
    match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "always_ask" | "ask" => Some(ApprovalMode::AlwaysAsk),
        "write" => Some(ApprovalMode::Write),
        "yolo" => Some(ApprovalMode::Yolo),
        _ => None,
    }
}

/// Human-readable label for an approval mode (the friendly spelling). Ports
/// `approvalModeLabel`.
#[must_use]
pub fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::AlwaysAsk => "always-ask",
        ApprovalMode::Write => "write",
        ApprovalMode::Yolo => "yolo",
    }
}

/// The model-list tool name for a provider, if one exists. Ports
/// `providerModelsTool`.
#[must_use]
pub fn provider_models_tool(provider: &str) -> Option<&'static str> {
    match provider.to_ascii_lowercase().as_str() {
        "xai" | "grok" => Some("xai_models"),
        "chatgpt" | "openai" | "openai_responses" => Some("openai_models"),
        _ => None,
    }
}

/// The agents a `/delegate` command accepts, matching the delegate runtime's
/// catalog names (DA-1/DA-2). Listed in the hint/help and rejected otherwise.
pub const DELEGATE_AGENTS: &[&str] = &["codex", "claude", "gemini"];

/// A parsed `/delegate [scout] <agent> [task...]` argument string: the role, the
/// validated agent name, plus the rest of the line as the task (empty when none
/// was supplied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateArgs {
    pub role: DelegateRole,
    pub agent: String,
    pub task: String,
}

/// Parse the `rest` of a `/delegate` command into a role + agent + task. An
/// optional leading `scout` keyword selects the read-only repository-explorer role
/// ([`DelegateRole::Scout`]); since `scout` is never a valid agent name the
/// detection is unambiguous (`/delegate scout <agent> <query>`). Otherwise the
/// first whitespace-delimited token is the agent (one of [`DELEGATE_AGENTS`]) and
/// the remainder is the task. `Err` carries a user-facing hint: a missing agent
/// or an unknown one.
///
/// # Errors
/// Returns a hint string when no agent token is present or it is not a known
/// delegate agent.
pub fn parse_delegate(rest: &str) -> Result<DelegateArgs, String> {
    let rest = rest.trim();
    // Peel an optional leading `scout` role keyword off the front.
    let (role, rest) = match rest.split_once(char::is_whitespace) {
        Some((first, tail)) if first.eq_ignore_ascii_case("scout") => {
            (DelegateRole::Scout, tail.trim())
        }
        // Bare `/delegate scout` (no agent): keep the role so the usage hint names
        // the missing agent rather than rejecting "scout" as an unknown agent.
        _ if rest.eq_ignore_ascii_case("scout") => (DelegateRole::Scout, ""),
        _ => (DelegateRole::Standard, rest),
    };
    let (agent, task) = match rest.split_once(char::is_whitespace) {
        Some((agent, task)) => (agent, task.trim()),
        None => (rest, ""),
    };
    if agent.is_empty() {
        return Err(format!(
            "usage: /delegate [scout] <agent> [task] — agent ∈ {}",
            DELEGATE_AGENTS.join("|")
        ));
    }
    let agent = agent.to_ascii_lowercase();
    if !DELEGATE_AGENTS.contains(&agent.as_str()) {
        return Err(format!(
            "unknown agent: {agent} — try {}",
            DELEGATE_AGENTS.join("|")
        ));
    }
    Ok(DelegateArgs {
        role,
        agent,
        task: task.to_string(),
    })
}

/// One model row extracted from a model-list tool result. Tolerant of shape:
/// a bare string, or an object with `id`/`slug`/`name` (+ optional `live`).
fn extract_model_rows(result: &Value) -> Vec<String> {
    let root = result
        .get("structuredContent")
        .filter(|v| v.is_object())
        .unwrap_or(result);
    let list = match root {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => match map.get("models") {
            Some(Value::Array(items)) => items.as_slice(),
            _ => &[],
        },
        _ => &[],
    };
    let mut rows = Vec::new();
    for item in list {
        match item {
            Value::String(s) => rows.push(s.clone()),
            Value::Object(map) => {
                let id = ["id", "slug", "name"]
                    .iter()
                    .find_map(|key| map.get(*key).and_then(Value::as_str));
                if let Some(id) = id {
                    let curated = map.get("live") == Some(&Value::Bool(false));
                    rows.push(format!("{id}{}", if curated { " (curated)" } else { "" }));
                }
            }
            _ => {}
        }
    }
    rows
}

/// Render a model-list tool result into a readable block. Tolerant of shape.
/// Ports `formatModels`.
#[must_use]
pub fn format_models(result: &Value) -> String {
    let rows = extract_model_rows(result);
    if rows.is_empty() {
        return "(no models returned)".to_string();
    }
    rows.iter()
        .map(|row| format!("  {row}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The chain-integrity verdict tone + line for a `ledger.verify` result. The
/// result Value is the engine's raw shape — intact:
/// `{ "ok": true, "count": N, "head_hash": "…" }`; tamper:
/// `{ "ok": false, "error": "<class>", "seq": K }` — but a host may wrap it under
/// `structuredContent`, so unwrap that defensively (mirrors `extract_model_rows`).
/// Read-only: this only renders the verdict the engine/CI/MCP already computed.
#[must_use]
pub fn format_ledger_verdict(result: &Value) -> (Tone, String) {
    let root = result
        .get("structuredContent")
        .filter(|v| v.is_object())
        .unwrap_or(result);
    if root.get("ok").and_then(Value::as_bool) == Some(true) {
        let count = root.get("count").and_then(Value::as_u64).unwrap_or(0);
        let head = root.get("head_hash").and_then(Value::as_str).unwrap_or("");
        let head8: String = head.chars().take(8).collect();
        let records = if count == 1 { "record" } else { "records" };
        return (
            Tone::Info,
            format!("ledger intact — {count} {records}, head {head8}"),
        );
    }
    let error = root
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let seq = root.get("seq").and_then(Value::as_u64);
    let at = seq.map_or_else(String::new, |k| format!(" at seq {k}"));
    (Tone::Error, format!("ledger TAMPERED — {error}{at}"))
}

/// A slash command offered by the autocomplete palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub hint: &'static str,
}

/// Slash commands offered by the autocomplete palette. Mirrors the TS `COMMANDS`.
pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "model",
        hint: "switch model (keeps history)",
    },
    CommandSpec {
        name: "provider",
        hint: "switch provider",
    },
    CommandSpec {
        name: "models",
        hint: "list provider models",
    },
    CommandSpec {
        name: "mode",
        hint: "approval mode: always-ask|write|yolo",
    },
    CommandSpec {
        name: "delegate",
        hint: "steerable agent; prefix `scout` for read-only explore",
    },
    CommandSpec {
        name: "done",
        hint: "end the active delegate session",
    },
    CommandSpec {
        name: "wechat",
        hint: "WeChat bridge: login|start|stop|status",
    },
    CommandSpec {
        name: "flow",
        hint: "run a fleet: parallel|vote|pipeline|--file",
    },
    CommandSpec {
        name: "new",
        hint: "fresh session (clears history)",
    },
    CommandSpec {
        name: "login",
        hint: "how to authenticate",
    },
    CommandSpec {
        name: "lease",
        hint: "show broker OAuth lease metadata (token redacted)",
    },
    CommandSpec {
        name: "ledger",
        hint: "verify the L1 evidence ledger's tamper-evident chain",
    },
    CommandSpec {
        name: "theme",
        hint: "cycle accent color",
    },
    CommandSpec {
        name: "help",
        hint: "show commands",
    },
    CommandSpec {
        name: "quit",
        hint: "close and exit",
    },
];

/// Commands matching a bare `/word` prefix; empty once the line has a space.
/// Ports `matchCommands` (the `^/(\w*)$` guard, lowercased prefix filter).
#[must_use]
pub fn match_commands(input: &str) -> Vec<CommandSpec> {
    let Some(rest) = input.strip_prefix('/') else {
        return Vec::new();
    };
    // The TS `\w*` is `[A-Za-z0-9_]*` and the `$` anchor means a space (or any
    // non-word char) ends matching — so a line with a space yields nothing.
    if !rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Vec::new();
    }
    let query = rest.to_ascii_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(&query))
        .copied()
        .collect()
}

/// The `/help` body. Mirrors the TS `HELP_TEXT`.
pub const HELP_TEXT: &str = "commands:\n  \
/model <id>                switch model (keeps history)\n  \
/provider <name> [model]   switch provider (claude|chatgpt|xai)\n  \
/models                    list the current provider's models\n  \
/mode [always-ask|write|yolo]  set the approval mode (bare = show current)\n  \
/delegate <agent> [task]   start a steerable delegate session (codex|claude|gemini)\n  \
/delegate scout <agent> <query>  read-only repo explorer → path:line citations\n  \
/done                      end the active delegate session (alias: /close)\n  \
/flow <strategy> <agents> <task>  run a fleet (parallel|vote|pipeline) · /flow --file <p.json>\n  \
/flow close                cancel the running flow\n  \
/new                       start a fresh session (clears history)\n  \
/login [provider] [--device]  how to authenticate; --device is reserved/fail-closed for now\n  \
/lease [provider] [--refresh]  show broker OAuth lease metadata; --refresh forces broker refresh; token redacted\n  \
/ledger                    verify the L1 evidence ledger's tamper-evident chain (read-only)\n  \
/wechat login [bot_type] [base_url]  start WeChat QR login (scan-only); scan the QR shown\n  \
/wechat start [agent] [autonomy] [owner1,owner2,...]  start bridge (default: claude, read_only, empty owners)\n  \
/wechat stop               stop the WeChat bridge\n  \
/wechat status             query bridge status\n  \
/theme                     cycle the accent color\n  \
/help                      show this help\n  \
/quit                      close the session and exit";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_command_splits_cmd_and_rest() {
        assert_eq!(
            parse_command("/model claude-opus"),
            Some(SlashCommand {
                cmd: "model".into(),
                rest: "claude-opus".into()
            })
        );
        assert_eq!(
            parse_command("/HELP"),
            Some(SlashCommand {
                cmd: "help".into(),
                rest: String::new()
            })
        );
        assert_eq!(parse_command("hello"), None);
        assert_eq!(
            parse_command("  /quit  "),
            Some(SlashCommand {
                cmd: "quit".into(),
                rest: String::new()
            })
        );
    }

    #[test]
    fn parse_approval_mode_accepts_friendly_spellings() {
        assert_eq!(
            parse_approval_mode("always-ask"),
            Some(ApprovalMode::AlwaysAsk)
        );
        assert_eq!(
            parse_approval_mode("always_ask"),
            Some(ApprovalMode::AlwaysAsk)
        );
        assert_eq!(parse_approval_mode("ASK"), Some(ApprovalMode::AlwaysAsk));
        assert_eq!(parse_approval_mode("write"), Some(ApprovalMode::Write));
        assert_eq!(parse_approval_mode("yolo"), Some(ApprovalMode::Yolo));
        assert_eq!(parse_approval_mode("nope"), None);
    }

    #[test]
    fn approval_mode_label_is_friendly() {
        assert_eq!(approval_mode_label(ApprovalMode::AlwaysAsk), "always-ask");
        assert_eq!(approval_mode_label(ApprovalMode::Write), "write");
        assert_eq!(approval_mode_label(ApprovalMode::Yolo), "yolo");
    }

    #[test]
    fn provider_models_tool_maps_known_providers() {
        assert_eq!(provider_models_tool("xai"), Some("xai_models"));
        assert_eq!(provider_models_tool("Grok"), Some("xai_models"));
        assert_eq!(provider_models_tool("chatgpt"), Some("openai_models"));
        assert_eq!(provider_models_tool("openai"), Some("openai_models"));
        assert_eq!(provider_models_tool("claude"), None);
    }

    #[test]
    fn match_commands_filters_by_bare_slash_prefix() {
        assert_eq!(
            match_commands("/m")
                .iter()
                .map(|c| c.name)
                .collect::<Vec<_>>(),
            vec!["model", "models", "mode"]
        );
        // "mode" is a prefix of "model"/"models", so /mode still lists all three.
        assert!(match_commands("/mode").iter().any(|c| c.name == "mode"));
        assert_eq!(match_commands("/model x").len(), 0);
        assert_eq!(match_commands("hi").len(), 0);
        assert!(match_commands("/").len() >= 5);
    }

    #[test]
    fn parse_delegate_splits_agent_and_task() {
        assert_eq!(
            parse_delegate("claude fix the bug"),
            Ok(DelegateArgs {
                role: DelegateRole::Standard,
                agent: "claude".into(),
                task: "fix the bug".into(),
            })
        );
        // Bare agent → empty task (the handler prompts for one).
        assert_eq!(
            parse_delegate("codex"),
            Ok(DelegateArgs {
                role: DelegateRole::Standard,
                agent: "codex".into(),
                task: String::new(),
            })
        );
        // Agent is lowercased to match the catalog names.
        assert_eq!(
            parse_delegate("GEMINI refactor").map(|d| d.agent),
            Ok("gemini".into())
        );
    }

    #[test]
    fn parse_delegate_recognizes_the_scout_role_keyword() {
        // A leading `scout` selects the read-only explorer role; the next token is
        // the agent and the remainder is the query.
        assert_eq!(
            parse_delegate("scout claude where is auth handled?"),
            Ok(DelegateArgs {
                role: DelegateRole::Scout,
                agent: "claude".into(),
                task: "where is auth handled?".into(),
            })
        );
        // Case-insensitive, and the agent is still validated.
        assert_eq!(
            parse_delegate("SCOUT gemini trace the parser").map(|d| d.role),
            Ok(DelegateRole::Scout)
        );
        // `scout` only counts as the role at the FRONT — a task may contain it.
        let args = parse_delegate("claude scout the module manually").unwrap();
        assert_eq!(args.role, DelegateRole::Standard);
        assert_eq!(args.task, "scout the module manually");
        // Bare `/delegate scout` (no agent) is a usage error, not "unknown agent scout".
        let err = parse_delegate("scout").expect_err("missing agent");
        assert!(err.contains("usage:"), "{err}");
    }

    #[test]
    fn parse_delegate_rejects_missing_and_unknown_agents() {
        assert!(parse_delegate("").is_err());
        assert!(parse_delegate("   ").is_err());
        let err = parse_delegate("opus do it").expect_err("unknown agent");
        assert!(err.contains("unknown agent: opus"), "{err}");
    }

    #[test]
    fn delegate_and_done_are_in_palette_and_help() {
        assert!(COMMANDS.iter().any(|c| c.name == "delegate"));
        assert!(COMMANDS.iter().any(|c| c.name == "done"));
        assert!(HELP_TEXT.contains("/delegate"));
        assert!(HELP_TEXT.contains("/done"));
    }

    #[test]
    fn flow_is_in_palette_and_help_with_close() {
        assert!(COMMANDS.iter().any(|c| c.name == "flow"));
        // The bare `/flow` prefix surfaces it in the palette.
        assert!(match_commands("/flow").iter().any(|c| c.name == "flow"));
        assert!(HELP_TEXT.contains("/flow"));
        assert!(HELP_TEXT.contains("/flow close"));
    }

    #[test]
    fn lease_is_in_palette_and_help() {
        assert!(COMMANDS.iter().any(|c| c.name == "lease"));
        assert!(match_commands("/lea").iter().any(|c| c.name == "lease"));
        assert!(HELP_TEXT.contains("/lease [provider]"));
    }

    #[test]
    fn wechat_is_in_palette_and_help() {
        assert!(COMMANDS.iter().any(|c| c.name == "wechat"));
        assert!(match_commands("/wec").iter().any(|c| c.name == "wechat"));
        assert!(HELP_TEXT.contains("/wechat login"));
        assert!(HELP_TEXT.contains("/wechat start"));
        assert!(HELP_TEXT.contains("/wechat stop"));
        assert!(HELP_TEXT.contains("/wechat status"));
    }

    #[test]
    fn format_models_renders_rows_and_curated_flag() {
        let result = json!({ "models": [
            { "id": "grok-4", "live": true },
            { "id": "grok-legacy", "live": false },
            "bare-string-model",
        ]});
        let out = format_models(&result);
        assert!(out.contains("  grok-4"));
        assert!(out.contains("  grok-legacy (curated)"));
        assert!(out.contains("  bare-string-model"));
    }

    #[test]
    fn ledger_is_in_palette_and_help() {
        assert!(COMMANDS.iter().any(|c| c.name == "ledger"));
        assert!(match_commands("/led").iter().any(|c| c.name == "ledger"));
        assert!(HELP_TEXT.contains("/ledger"));
    }

    #[test]
    fn format_ledger_verdict_renders_intact_chain() {
        // Intact verdict: count + first-8 of the head hash, Info tone. Pluralizes
        // "records" and unwraps a `structuredContent` wrapper defensively.
        let (tone, line) = format_ledger_verdict(&json!({
            "ok": true, "count": 3, "head_hash": "abcdef0123456789",
        }));
        assert_eq!(tone, Tone::Info);
        assert_eq!(line, "ledger intact — 3 records, head abcdef01");
        // A single record reads "record", not "records".
        let (_, one) = format_ledger_verdict(&json!({
            "ok": true, "count": 1, "head_hash": "ff00",
        }));
        assert_eq!(one, "ledger intact — 1 record, head ff00");
        // Nested under structuredContent is unwrapped.
        let (_, nested) = format_ledger_verdict(&json!({
            "structuredContent": { "ok": true, "count": 2, "head_hash": "deadbeefcafe" },
        }));
        assert_eq!(nested, "ledger intact — 2 records, head deadbeef");
    }

    #[test]
    fn format_ledger_verdict_renders_tampered_chain() {
        // Tamper verdict: error class + the offending seq, Error tone.
        let (tone, line) = format_ledger_verdict(&json!({
            "ok": false, "error": "HashMismatch", "seq": 4,
        }));
        assert_eq!(tone, Tone::Error);
        assert_eq!(line, "ledger TAMPERED — HashMismatch at seq 4");
        // A tamper without a seq still reads cleanly.
        let (_, no_seq) = format_ledger_verdict(&json!({
            "ok": false, "error": "PrevMismatch",
        }));
        assert_eq!(no_seq, "ledger TAMPERED — PrevMismatch");
    }

    #[test]
    fn format_models_handles_structured_content_and_empties() {
        let nested = json!({ "structuredContent": { "models": [ { "slug": "x" } ] } });
        assert!(format_models(&nested).contains("  x"));
        assert_eq!(format_models(&json!({})), "(no models returned)");
        assert_eq!(format_models(&json!([])), "(no models returned)");
    }
}
