//! Pure slash-command helpers: parsing, the autocomplete palette, approval-mode
//! spellings, and model-list formatting.

use nerve_runtime::ApprovalMode;
use serde_json::Value;

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

/// A parsed `/delegate <agent> [task...]` argument string: the validated agent
/// name plus the rest of the line as the task (empty when none was supplied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegateArgs {
    pub agent: String,
    pub task: String,
}

/// Parse the `rest` of a `/delegate` command into an agent + task. The first
/// whitespace-delimited token is the agent (must be one of [`DELEGATE_AGENTS`]);
/// the remainder is the task. `Err` carries a user-facing hint: a missing agent
/// or an unknown one.
///
/// # Errors
/// Returns a hint string when no agent token is present or it is not a known
/// delegate agent.
pub fn parse_delegate(rest: &str) -> Result<DelegateArgs, String> {
    let rest = rest.trim();
    let (agent, task) = match rest.split_once(char::is_whitespace) {
        Some((agent, task)) => (agent, task.trim()),
        None => (rest, ""),
    };
    if agent.is_empty() {
        return Err(format!(
            "usage: /delegate <agent> [task] — agent ∈ {}",
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
        hint: "start a steerable agent (codex|claude|gemini)",
    },
    CommandSpec {
        name: "done",
        hint: "end the active delegate session",
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
/done                      end the active delegate session (alias: /close)\n  \
/new                       start a fresh session (clears history)\n  \
/login [provider]          how to authenticate a provider\n  \
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
                agent: "claude".into(),
                task: "fix the bug".into(),
            })
        );
        // Bare agent → empty task (the handler prompts for one).
        assert_eq!(
            parse_delegate("codex"),
            Ok(DelegateArgs {
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
    fn format_models_handles_structured_content_and_empties() {
        let nested = json!({ "structuredContent": { "models": [ { "slug": "x" } ] } });
        assert!(format_models(&nested).contains("  x"));
        assert_eq!(format_models(&json!({})), "(no models returned)");
        assert_eq!(format_models(&json!([])), "(no models returned)");
    }
}
