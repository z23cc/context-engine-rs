//! GUI data fetchers: read real workspace / git / file-tree facts via the
//! `tool.call` command (the daemon's snapshot-backed tools), so the chrome
//! shows live data instead of placeholders. Each helper runs one `tool.call`
//! job to completion and returns the tool's `structuredContent`. Kept out of
//! `app.rs` to stay under the file-size gate.

use crate::rpc::{rpc_call, start_job_await};
use nerve_proto::HostCapabilities;
use nerve_proto::protocol::RuntimeInfo;
use serde_json::{Value, json};

/// The local agent CLIs the GUI drives over the delegate path: `(id, label)`.
/// `id` is the catalog name `delegate.start` accepts (claude / codex / gemini);
/// each runs the user's logged-in CLI, which owns its own model/credentials.
pub const AGENTS: &[(&str, &str)] = &[
    ("claude", "Claude Code"),
    ("codex", "Codex"),
    // gemini is one-shot (not a parked/steerable session), so it does not fit the
    // steerable-chat model yet — omitted until one-shot turns are handled.
];

/// The human label for an agent id (falls back to the id).
pub fn agent_label(id: &str) -> &str {
    AGENTS
        .iter()
        .find(|(a, _)| *a == id)
        .map(|(_, label)| *label)
        .unwrap_or(id)
}

/// Per-agent model catalog `(agent, model-id, label)`, modeled on RepoPrompt CE's
/// AgentModel list. `model-id == ""` means "the CLI's own configured default"
/// (delegate.start sends no `model`). Passed verbatim to the CLI (`claude --model`
/// / codex thread-start `model`), which accepts any string — keep current.
/// (Codex reasoning-effort levels are encoded in the model string upstream; Nerve's
/// codex thread-start has no effort field yet, so base models are offered for now.)
pub const AGENT_MODELS: &[(&str, &str, &str)] = &[
    ("claude", "", "Default"),
    ("claude", "sonnet", "Sonnet (latest)"),
    ("claude", "opus", "Opus (latest)"),
    ("claude", "opus[1m]", "Opus (latest, 1M)"),
    ("claude", "haiku", "Haiku (latest)"),
    ("claude", "claude-sonnet-4-6", "Sonnet 4.6"),
    ("claude", "claude-opus-4-7", "Opus 4.7"),
    ("claude", "claude-haiku-4-5", "Haiku 4.5"),
    ("claude", "claude-fable-5", "Fable 5"),
    // Codex models carry the reasoning effort as a suffix (RepoPrompt encoding);
    // Nerve's codex thread-start splits it into model + model_reasoning_effort.
    ("codex", "", "Default"),
    ("codex", "gpt-5.5-low", "5.5 · Low"),
    ("codex", "gpt-5.5-medium", "5.5 · Medium"),
    ("codex", "gpt-5.5-high", "5.5 · High"),
    ("codex", "gpt-5.5-xhigh", "5.5 · XHigh"),
    ("codex", "gpt-5.3-codex-medium", "5.3 · Medium"),
    ("codex", "gpt-5.3-codex-high", "5.3 · High"),
];

/// Human label for the selected model id within an agent catalog.
pub fn model_label<'a>(agent: &str, model: &'a str) -> &'a str {
    AGENT_MODELS
        .iter()
        .find(|(a, id, _)| *a == agent && *id == model)
        .map_or(
            if model.is_empty() { "Default" } else { model },
            |(_, _, label)| *label,
        )
}

pub fn model_control_label(agent: &str, model: &str) -> String {
    format!(
        "Model for {}: {}",
        agent_label(agent),
        model_label(agent, model)
    )
}

/// A short sidebar title from the first user message.
pub(crate) fn truncate_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    let mut title: String = line.chars().take(40).collect();
    if line.chars().count() > 40 {
        title.push('…');
    }
    if title.is_empty() {
        "New thread".into()
    } else {
        title
    }
}

pub async fn fetch_host_capabilities(token: &str) -> Result<HostCapabilities, String> {
    let result = start_job_await(token, crate::command::host_capabilities()).await?;
    serde_json::from_value::<HostCapabilities>(result)
        .map_err(|err| format!("Invalid capability response: {err}"))
}

/// The daemon's live runtime protocol version (`runtime/info` → `protocolVersion`,
/// e.g. `"7"`). `None` if the call fails, so the sidebar can show a neutral
/// "runtime" label rather than a stale hardcoded number.
pub async fn fetch_protocol_version(token: &str) -> Option<String> {
    rpc_call::<RuntimeInfo>(token, "runtime/info", json!({}))
        .await
        .ok()
        .map(|info| info.protocol_version)
}

/// The L1 evidence-ledger chain-integrity verdict, parsed from `ledger.verify`'s
/// result `Value` for the sidebar badge. Intact => `{ok:true, count, head_hash}`;
/// tamper => `{ok:false, error:"<HashMismatch|SeqGap|PrevMismatch>", seq}`.
#[derive(Clone, Default)]
pub struct LedgerIntegrity {
    pub ok: bool,
    pub count: u64,
    pub head: Option<String>,
    pub error: Option<String>,
}

/// Re-derive the append-only evidence ledger's hash chain via the daemon's
/// read-only `ledger.verify` job, mirroring [`fetch_protocol_version`] so the
/// sidebar can surface the same tamper-evident verdict CI/MCP get. `None` if the
/// job fails to run, so the badge can stay neutral rather than show a stale state.
pub async fn fetch_ledger_integrity(token: &str) -> Option<LedgerIntegrity> {
    let value = start_job_await(token, crate::command::ledger_verify())
        .await
        .ok()?;
    Some(LedgerIntegrity {
        ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
        count: value.get("count").and_then(Value::as_u64).unwrap_or(0),
        head: value
            .get("head_hash")
            .and_then(Value::as_str)
            .map(str::to_string),
        error: value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub async fn pick_host_folder(token: &str, title: &str) -> Result<String, String> {
    let result = start_job_await(token, crate::command::host_folder_pick(title)).await?;
    result
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "folder picker returned no path".to_string())
}

pub async fn save_host_text_file(
    token: &str,
    title: &str,
    default_name: &str,
    text: String,
) -> Result<String, String> {
    let result = start_job_await(
        token,
        crate::command::host_file_save_text(title, default_name, text),
    )
    .await?;
    result
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "save panel returned no path".to_string())
}

pub async fn open_host_url(token: &str, url: &str) -> Result<(), String> {
    let result = start_job_await(token, crate::command::host_url_open(url)).await?;
    if result
        .get("opened")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        Err("host URL opener returned no success marker".into())
    }
}

/// Run one `tool.call` job to completion; return its `structuredContent` object.
pub async fn tool_job(token: &str, name: &str, arguments: Value) -> Result<Value, String> {
    let result = start_job_await(token, crate::command::tool_call(name, arguments)).await?;
    result
        .get("structuredContent")
        .cloned()
        .ok_or_else(|| format!("{name}: no structuredContent"))
}

/// Run one `tool.call` and return BOTH the assembled `content[0].text` and the
/// `structuredContent` (some tools, like `workspace_context`, put the rendered
/// text in `content` and only the breakdown in `structuredContent`).
pub async fn tool_job_full(
    token: &str,
    name: &str,
    arguments: Value,
) -> Result<(String, Value), String> {
    let result = start_job_await(token, crate::command::tool_call(name, arguments)).await?;
    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default()
        .to_string();
    let structured = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    Ok((text, structured))
}

/// Assemble context for a recipe; returns `(rendered text, token-breakdown JSON)`.
pub async fn fetch_context(
    token: &str,
    recipe: &str,
    git_diff: Option<String>,
    workspace: &str,
) -> Option<(String, Value)> {
    let mut args = json!({ "recipe": recipe });
    if let Some(diff) = git_diff {
        args["git_diff"] = json!(diff);
    }
    tool_job_full(token, "workspace_context", with_ws(args, workspace))
        .await
        .ok()
}

#[derive(Clone, Default, serde::Deserialize)]
pub struct SelectionRange {
    #[serde(default)]
    pub start_line: usize,
    #[serde(default)]
    pub end_line: usize,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Default, serde::Deserialize)]
pub struct SelectionFile {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub display_path: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub ranges: Vec<SelectionRange>,
    #[serde(default)]
    pub token_estimate: usize,
}

#[derive(Clone, Default, serde::Deserialize)]
pub struct SelectionSummary {
    #[serde(default)]
    pub files: Vec<SelectionFile>,
    #[serde(default)]
    pub total_tokens: usize,
}

/// Run a `manage_selection` op (get/add/remove/clear); returns its
/// `structuredContent` (the selection summary with per-file token counts).
pub async fn selection_op(
    token: &str,
    op: &str,
    paths: Vec<String>,
    workspace: &str,
) -> Option<Value> {
    tool_job(
        token,
        "manage_selection",
        with_ws(json!({ "op": op, "paths": paths }), workspace),
    )
    .await
    .ok()
}

pub async fn selection_summary(token: &str, workspace: &str) -> SelectionSummary {
    tool_job(
        token,
        "manage_selection",
        with_ws(json!({ "op": "get" }), workspace),
    )
    .await
    .ok()
    .and_then(|sc| serde_json::from_value::<SelectionSummary>(sc).ok())
    .unwrap_or_default()
}

/// Attach the active `workspace` selector to a tool's arguments so the dispatch
/// routes the call to that workspace (see `workspace_arg`); a blank name leaves
/// the call on the default workspace.
fn with_ws(mut args: Value, workspace: &str) -> Value {
    if !workspace.is_empty() {
        args["workspace"] = json!(workspace);
    }
    args
}

/// One row of a `manage_workspaces` response: `(name, first-root)`.
fn parse_workspaces(sc: &Value) -> Vec<(String, String)> {
    sc.get("workspaces")
        .and_then(|w| w.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|ws| {
                    let name = ws.get("name")?.as_str()?.to_string();
                    let root = ws
                        .get("roots")
                        .and_then(|r| r.as_array())
                        .and_then(|r| r.first())
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    Some((name, root))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// All registered workspaces as `(name, first-root)`, via `manage_workspaces {op:list}`.
pub async fn list_workspaces(token: &str) -> Vec<(String, String)> {
    match tool_job(token, "manage_workspaces", json!({"op":"list"})).await {
        Ok(sc) => parse_workspaces(&sc),
        Err(_) => Vec::new(),
    }
}

/// Register a workspace root (`manage_workspaces {op:add}`); returns the new list.
pub async fn add_workspace(token: &str, name: &str, root: &str) -> Vec<(String, String)> {
    let _ = tool_job(
        token,
        "manage_workspaces",
        json!({"op":"add","name":name,"roots":[root]}),
    )
    .await;
    list_workspaces(token).await
}

/// Remove a registered workspace (`manage_workspaces {op:remove}`); returns the new list.
pub async fn remove_workspace(token: &str, name: &str) -> Vec<(String, String)> {
    let _ = tool_job(
        token,
        "manage_workspaces",
        json!({"op":"remove","name":name}),
    )
    .await;
    list_workspaces(token).await
}

/// The current git branch, parsed from the `git {op:status}` porcelain header
/// (`## <branch>...origin/<branch> [ahead N]`).
pub async fn fetch_branch(token: &str, workspace: &str) -> Option<String> {
    let sc = tool_job(token, "git", with_ws(json!({"op":"status"}), workspace))
        .await
        .ok()?;
    let output = sc.get("output")?.as_str()?;
    let header = output.lines().next()?.strip_prefix("## ")?;
    let branch = header
        .split_once("...")
        .map(|(b, _)| b)
        .unwrap_or(header)
        .split_whitespace()
        .next()
        .unwrap_or(header);
    Some(branch.to_string())
}

/// The repo file tree (ASCII), via `get_file_tree`.
pub async fn fetch_file_tree(token: &str, workspace: &str) -> Option<String> {
    let sc = tool_job(
        token,
        "get_file_tree",
        with_ws(json!({"mode":"auto","max_depth":3}), workspace),
    )
    .await
    .ok()?;
    sc.get("tree")?.as_str().map(str::to_string)
}

/// The working-tree diff (unified), via `git {op:diff}`. Returns the diff text,
/// or a short note when the tree is clean.
pub async fn fetch_diff(token: &str, workspace: &str) -> Option<String> {
    let sc = tool_job(token, "git", with_ws(json!({"op":"diff"}), workspace))
        .await
        .ok()?;
    let out = sc.get("output")?.as_str()?.trim();
    Some(if out.is_empty() {
        "No changes in the working tree.".to_string()
    } else {
        out.to_string()
    })
}

/// One file row for the clickable picker (from `list_files`).
#[derive(Clone, serde::Deserialize)]
pub struct FileRow {
    pub path: String,
    pub display_path: String,
    pub selected: bool,
}

/// Structured, selection-aware file list for the picker; returns `(rows, truncated)`.
pub async fn list_files(
    token: &str,
    query: &str,
    limit: usize,
    workspace: &str,
) -> (Vec<FileRow>, bool) {
    let mut args = json!({ "limit": limit });
    if !query.is_empty() {
        args["query"] = json!(query);
    }
    match tool_job(token, "list_files", with_ws(args, workspace)).await {
        Ok(sc) => {
            let rows = sc
                .get("files")
                .cloned()
                .and_then(|f| serde_json::from_value::<Vec<FileRow>>(f).ok())
                .unwrap_or_default();
            let truncated = sc
                .get("truncated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            (rows, truncated)
        }
        Err(_) => (Vec::new(), false),
    }
}
