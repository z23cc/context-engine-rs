//! Persisted run defaults + interactive provider/model selection.
//!
//! claude/codex-style CLIs launch with no flags, keep a persisted default model,
//! and let you pick interactively the first time. This module is that layer for
//! `nerve chat` and `nerve agent run`: it reads/writes a user-owned
//! `config_home()/config.json` and resolves `(provider, model)` with the
//! precedence flag -> caller default (e.g. agent-def) -> config -> TTY picker.
//!
//! No protocol or core involvement — composition-root policy that reads the
//! credential store (to know which providers are logged in) and writes a
//! declarative config file. See docs/designs/agent-config-and-model-selection.md.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use nerve_agent::ProviderId;
use nerve_agent::auth::{self, config_home};
use serde::{Deserialize, Serialize};

/// User-owned run defaults, persisted as `config_home()/config.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct RunConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) default_model: Option<String>,
}

/// Built-in providers: `(id, canonical --provider name, model hint)`.
const BUILTINS: &[(ProviderId, &str, &str)] = &[
    (ProviderId::Anthropic, "claude", "claude-sonnet-4"),
    (ProviderId::OpenAi, "chatgpt", "gpt-5.5"),
    (ProviderId::Xai, "xai", "grok-4-fast"),
];

fn config_path() -> Result<PathBuf> {
    Ok(config_home()
        .map_err(|err| anyhow!("config home: {err}"))?
        .join("config.json"))
}

/// Load run defaults; a missing / unreadable / malformed file yields empty
/// defaults rather than an error (config is best-effort, never fatal).
pub(crate) fn load() -> RunConfig {
    config_path()
        .map(|path| load_from(&path))
        .unwrap_or_default()
}

fn load_from(path: &Path) -> RunConfig {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Persist run defaults (pretty JSON), creating the config home if needed.
pub(crate) fn save(config: &RunConfig) -> Result<()> {
    save_to(&config_path()?, config)
}

fn save_to(path: &Path, config: &RunConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| anyhow!("create {}: {err}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(config)?;
    std::fs::write(path, json).map_err(|err| anyhow!("write {}: {err}", path.display()))
}

/// Merge explicit values over persisted defaults. `None` means "still unknown".
fn decide(
    provider: Option<String>,
    model: Option<String>,
    config: &RunConfig,
) -> Option<(String, String)> {
    let provider = provider.or_else(|| config.default_provider.clone());
    let model = model.or_else(|| config.default_model.clone());
    match (provider, model) {
        (Some(provider), Some(model)) => Some((provider, model)),
        _ => None,
    }
}

/// Resolve `(provider, model)`: explicit values win; else the persisted default;
/// else (on a TTY, when `interactive`) an interactive picker whose choice is
/// persisted; else an actionable error.
pub(crate) fn resolve(
    provider: Option<String>,
    model: Option<String>,
    interactive: bool,
) -> Result<(String, String)> {
    let config = load();
    if let Some(resolved) = decide(provider.clone(), model.clone(), &config) {
        return Ok(resolved);
    }
    if interactive && std::io::stdin().is_terminal() {
        let picked = pick(
            provider.or(config.default_provider),
            model.or(config.default_model),
        )?;
        if let Err(err) = save(&RunConfig {
            default_provider: Some(picked.0.clone()),
            default_model: Some(picked.1.clone()),
        }) {
            eprintln!("\u{26a0}  could not save default model: {err}");
        }
        return Ok(picked);
    }
    bail!(
        "no provider/model: pass --provider and --model, or run interactively to \
         configure a default (run `nerve agent login` first)"
    )
}

/// Interactive picker; pre-filled sides are not prompted.
fn pick(provider: Option<String>, model: Option<String>) -> Result<(String, String)> {
    let provider = match provider {
        Some(provider) => provider,
        None => pick_provider()?,
    };
    let model = match model {
        Some(model) => model,
        None => prompt_model(&provider)?,
    };
    Ok((provider, model))
}

/// List logged-in built-in providers and prompt for one.
fn pick_provider() -> Result<String> {
    let available: Vec<&str> = BUILTINS
        .iter()
        .filter(|entry| matches!(auth::load_credential(entry.0), Ok(Some(_))))
        .map(|entry| entry.1)
        .collect();
    match available.as_slice() {
        [] => {
            bail!("no providers logged in; run `nerve agent login --provider claude|chatgpt|xai`")
        }
        [only] => {
            eprintln!("using the only logged-in provider: {only}");
            Ok((*only).to_string())
        }
        many => choose_provider(many),
    }
}

fn choose_provider(available: &[&str]) -> Result<String> {
    eprintln!("select a provider:");
    for (idx, name) in available.iter().enumerate() {
        eprintln!("  {}) {name}", idx + 1);
    }
    let choice = prompt_line("provider [1]: ")?;
    let index = choice
        .trim()
        .parse::<usize>()
        .unwrap_or(1)
        .saturating_sub(1);
    available
        .get(index)
        .map(|name| (*name).to_string())
        .ok_or_else(|| anyhow!("invalid selection: {}", choice.trim()))
}

/// Prompt for a model id, accepting the built-in hint on an empty line.
fn prompt_model(provider: &str) -> Result<String> {
    let hint = BUILTINS
        .iter()
        .find(|entry| entry.1 == provider)
        .map_or("", |entry| entry.2);
    let prompt = if hint.is_empty() {
        format!("model id for {provider}: ")
    } else {
        format!("model id for {provider} [{hint}]: ")
    };
    let model = prompt_line(&prompt)?;
    let model = model.trim();
    if !model.is_empty() {
        return Ok(model.to_string());
    }
    if hint.is_empty() {
        bail!("a model id is required");
    }
    Ok(hint.to_string())
}

/// Write a prompt to stderr and read one line from stdin.
fn prompt_line(prompt: &str) -> Result<String> {
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(prompt.as_bytes());
    let _ = stderr.flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 {
        bail!("no input (reached end of stdin)");
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_prefers_explicit_then_config() {
        let cfg = RunConfig {
            default_provider: Some("xai".into()),
            default_model: Some("grok-4-fast".into()),
        };
        assert_eq!(
            decide(Some("claude".into()), Some("m".into()), &cfg),
            Some(("claude".into(), "m".into()))
        );
        assert_eq!(
            decide(None, None, &cfg),
            Some(("xai".into(), "grok-4-fast".into()))
        );
        assert_eq!(
            decide(Some("chatgpt".into()), None, &cfg),
            Some(("chatgpt".into(), "grok-4-fast".into()))
        );
    }

    #[test]
    fn decide_none_when_incomplete() {
        let empty = RunConfig::default();
        assert_eq!(decide(None, None, &empty), None);
        assert_eq!(decide(Some("claude".into()), None, &empty), None);
        assert_eq!(decide(None, Some("m".into()), &empty), None);
    }

    #[test]
    fn save_load_round_trips() {
        let path =
            std::env::temp_dir().join(format!("nerve-runconfig-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = RunConfig {
            default_provider: Some("claude".into()),
            default_model: Some("claude-sonnet-4".into()),
        };
        save_to(&path, &cfg).expect("save");
        let loaded = load_from(&path);
        assert_eq!(loaded.default_provider.as_deref(), Some("claude"));
        assert_eq!(loaded.default_model.as_deref(), Some("claude-sonnet-4"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_missing_or_garbage_is_default() {
        let missing = std::env::temp_dir().join("nerve-runconfig-absent-zzz.json");
        let _ = std::fs::remove_file(&missing);
        assert!(load_from(&missing).default_provider.is_none());

        let garbage = std::env::temp_dir().join(format!(
            "nerve-runconfig-garbage-{}.json",
            std::process::id()
        ));
        std::fs::write(&garbage, b"not json{{").expect("write garbage");
        assert!(load_from(&garbage).default_model.is_none());
        let _ = std::fs::remove_file(&garbage);
    }
}
