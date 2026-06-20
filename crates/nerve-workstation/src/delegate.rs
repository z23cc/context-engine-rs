//! External-agent delegation: the agent catalog + the read-only `list_agents`
//! discovery tool.
//!
//! Modeled on RepoPrompt CE's `agent_manage list_agents`. DA-1 ships the catalog
//! and a PATH-probe discovery tool; the actual subprocess driving is DA-2 (the
//! `delegate.start` job, executed by the host delegate runtime in `jobs.rs`).
//!
//! Probe discipline: `list_agents` resolves each candidate binary on `PATH` and
//! checks it is an executable regular file — it never *spawns* the binary (no
//! `--version`, no network, no side effects), mirroring a `which` lookup. This
//! keeps discovery cheap, deterministic, and side-effect-free.

use crate::worker::{WorkerDefKind, WorkerRegistry};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// One entry in the hardcoded external-agent catalog: the catalog `name` (also
/// the value passed as `delegate.start`'s `agent`) and the binary to resolve on
/// `PATH`. Every catalog agent supports the same three autonomy modes, mapped to
/// each vendor's sandbox/permission flag by DA-2.
struct AgentSpec {
    /// Catalog name, used as the `agent` argument to `delegate.start`.
    name: &'static str,
    /// Executable resolved on `PATH` to determine availability.
    binary: &'static str,
}

/// The hardcoded external-agent catalog (codex / claude / gemini), mirroring
/// RepoPrompt CE. Availability is probed at call time; the catalog itself is
/// static.
const AGENT_CATALOG: &[AgentSpec] = &[
    AgentSpec {
        name: "codex",
        binary: "codex",
    },
    AgentSpec {
        name: "claude",
        binary: "claude",
    },
    AgentSpec {
        name: "gemini",
        binary: "gemini",
    },
];

/// Autonomy modes every catalog agent accepts, surfaced so a client can present
/// the choice. The wire strings match [`nerve_runtime::DelegateAutonomy`]'s serde
/// names (`read_only` / `edit` / `full`).
const AUTONOMY_MODES: &[&str] = &["read_only", "edit", "full"];

/// MCP-style spec for the `list_agents` tool. Read-only and side-effect-free:
/// the optional `refresh` flag is accepted for forward-compatibility (DA-1 holds
/// no cache, so it is a no-op) and never changes the catalog.
#[must_use]
pub(crate) fn tool_specs() -> Vec<Value> {
    vec![json!({
        "name": "list_agents",
        "description": "List external coding agents Nerve can delegate to (codex / claude / \
            gemini), probing each binary on PATH for availability, plus the worker-as-data \
            catalog (`.nerve/workers/*.json`, project > global > built-in) the orchestrator's \
            named workers resolve through. Read-only; does not spawn the agents.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "refresh": {
                    "type": "boolean",
                    "description": "Reserved for forward-compatibility; DA-1 holds no cache so \
                        this is a no-op."
                }
            },
            "additionalProperties": false
        }
    })]
}

/// Handle a `tools/call` for `list_agents`. Returns `Ok(None)` for any other tool
/// so runtime dispatch continues to the next adapter / core. `root` is the resolved
/// workspace root used to discover the C6 worker-as-data catalog (`None` → built-ins
/// only).
#[must_use]
pub(crate) fn handle_tool_call(params: &Value, root: Option<&Path>) -> Option<Value> {
    let name = params.get("name").and_then(Value::as_str);
    if name != Some("list_agents") {
        return None;
    }
    let agents: Vec<Value> = AGENT_CATALOG.iter().map(probe_agent).collect();
    Some(json!({ "agents": agents, "workers": worker_catalog(root) }))
}

/// The C6 worker-as-data catalog for `list_agents`: every discovered [`WorkerDef`]
/// with its winning source (built-in / global / project), so the catalog reflects
/// "add any worker is a data change". Sorted by name (the registry sorts) for
/// deterministic output; the built-in cli{codex|claude|gemini} are always present.
fn worker_catalog(root: Option<&Path>) -> Vec<Value> {
    WorkerRegistry::discover(root)
        .catalog()
        .into_iter()
        .map(|worker| {
            json!({
                "name": worker.name,
                "kind": worker_kind_label(&worker.def.kind),
                "source": worker.source.to_string(),
            })
        })
        .collect()
}

/// A stable one-word label for a [`WorkerDefKind`] (the worker-as-data shape), shown
/// in the `list_agents` worker catalog.
fn worker_kind_label(kind: &WorkerDefKind) -> &'static str {
    match kind {
        WorkerDefKind::Cli { .. } => "cli",
        WorkerDefKind::Provider { .. } => "provider",
        WorkerDefKind::Remote { .. } => "remote",
        WorkerDefKind::Mcp { .. } => "mcp",
    }
}

/// Resolve one catalog agent's availability by a PATH lookup (no spawn) and
/// render it as the protocol shape. For codex, also surface the DA-6 MCP allowlist
/// view (`mcp_servers`) so a client can see which servers a delegated codex would
/// boot.
fn probe_agent(spec: &AgentSpec) -> Value {
    let path = resolve_on_path(spec.binary);
    let mut entry = json!({
        "name": spec.name,
        "binary": spec.binary,
        "available": path.is_some(),
        "path": path.as_ref().map(|p| p.display().to_string()),
        "autonomy_modes": AUTONOMY_MODES,
    });
    if spec.name == "codex" {
        entry["mcp_servers"] = codex_mcp_servers_view();
    }
    entry
}

/// The read-only DA-6 MCP view for the codex agent entry: each discovered
/// `[mcp_servers.<name>]` from `~/.codex/config.toml` with `enabled = name ∈
/// effective config allowlist` (`toggleable: true`), followed by codex's built-in
/// plugin/app servers marked `toggleable: false` (the `mcp_servers.*` override
/// cannot disable them — DA-6 out of scope). Uses the **config** allowlist; a
/// `delegate.start` may still override it per call.
fn codex_mcp_servers_view() -> Value {
    let allowlist: std::collections::BTreeSet<String> = crate::runconfig::codex_mcp_allowlist()
        .into_iter()
        .collect();
    let mut servers: Vec<Value> = crate::delegate_codex_mcp::discover_codex_mcp_servers()
        .into_iter()
        .map(|name| {
            let enabled = allowlist.contains(&name);
            json!({ "name": name, "enabled": enabled, "toggleable": true })
        })
        .collect();
    // The built-in plugin/app servers are always present and non-toggleable: the
    // `mcp_servers.*` override does not reach them, so a client should not expect
    // the allowlist to affect them.
    servers.extend(
        crate::delegate_codex_mcp::NON_TOGGLEABLE_PLUGIN_SERVERS
            .iter()
            .map(|name| json!({ "name": name, "enabled": true, "toggleable": false })),
    );
    Value::Array(servers)
}

/// Resolve `binary` against the `PATH` environment variable, returning the first
/// matching executable regular file. A `which`-style probe: it checks existence
/// and the executable bit (on Unix) but never runs the binary.
fn resolve_on_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable_file(candidate))
}

/// Whether `path` is a regular file that is executable. On Unix this checks the
/// owner/group/other execute bits; on other platforms, existence as a file is
/// sufficient (the OS enforces executability at spawn time).
fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_spec_advertises_list_agents() {
        let specs = tool_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0]["name"], "list_agents");
    }

    #[test]
    fn non_list_agents_call_is_not_claimed() {
        assert!(handle_tool_call(&json!({ "name": "read_file" }), None).is_none());
        assert!(handle_tool_call(&json!({ "arguments": {} }), None).is_none());
    }

    #[test]
    fn list_agents_returns_the_full_catalog_shape() {
        let result = handle_tool_call(&json!({ "name": "list_agents" }), None).expect("claimed");
        let agents = result["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), AGENT_CATALOG.len());
        let names: Vec<_> = agents.iter().filter_map(|a| a["name"].as_str()).collect();
        assert_eq!(names, vec!["codex", "claude", "gemini"]);
        for agent in agents {
            assert!(agent["binary"].is_string());
            assert!(agent["available"].is_boolean());
            // `path` is a string when available, null otherwise.
            assert!(agent["path"].is_string() || agent["path"].is_null());
            assert_eq!(
                agent["available"].as_bool().unwrap(),
                !agent["path"].is_null()
            );
            assert_eq!(
                agent["autonomy_modes"],
                json!(["read_only", "edit", "full"])
            );
        }
    }

    #[test]
    fn list_agents_surfaces_the_worker_catalog_with_sources() {
        // The C6 worker-as-data catalog appears under `workers`, each with its source.
        // The built-in cli{codex,claude,gemini} are always present (and are cli kind).
        let result = handle_tool_call(&json!({ "name": "list_agents" }), None).expect("claimed");
        let workers = result["workers"].as_array().expect("workers array");
        let by_name: std::collections::BTreeMap<&str, &str> = workers
            .iter()
            .map(|w| (w["name"].as_str().unwrap(), w["kind"].as_str().unwrap()))
            .collect();
        for name in ["codex", "claude", "gemini"] {
            // Present as a cli worker (don't assume the host's global dir is empty, so
            // assert the entry exists + is a cli kind rather than its exact source).
            assert_eq!(by_name.get(name), Some(&"cli"), "built-in {name} missing");
        }
        for worker in workers {
            assert!(worker["source"].is_string(), "every worker has a source");
        }
    }

    #[test]
    fn worker_catalog_reports_a_project_def_with_its_source() {
        // Hermetic: a project `.nerve/workers/reviewer.json` shows up as `project`.
        // `worker_catalog` calls `WorkerRegistry::discover`, which joins `<root>/.nerve`.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join(".nerve")
            .join("workers")
            .join("reviewer.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{ "kind": { "type": "provider", "provider": "xai", "model": "grok" } }"#,
        )
        .unwrap();
        let catalog = worker_catalog(Some(dir.path()));
        let reviewer = catalog
            .iter()
            .find(|w| w["name"] == "reviewer")
            .expect("reviewer in catalog");
        assert_eq!(reviewer["kind"], "provider");
        assert_eq!(reviewer["source"], "project");
    }

    #[test]
    fn list_agents_codex_entry_carries_mcp_servers_view() {
        // DA-6: the codex entry surfaces an `mcp_servers` allowlist view; the other
        // agents do not. Discovery is pinned to {chrome-devtools, xcodebuildmcp} and
        // the config allowlist opts chrome-devtools in (thread-local overrides, so no
        // process-env mutation — env writes race with concurrent readers).
        let result = crate::delegate_codex_mcp::test_override::with(
            &["chrome-devtools", "xcodebuildmcp"],
            || {
                crate::runconfig::codex_allowlist_override::with(&["chrome-devtools"], || {
                    handle_tool_call(&json!({ "name": "list_agents" }), None).expect("claimed")
                })
            },
        );
        let agents = result["agents"].as_array().expect("agents array");
        let codex = agents.iter().find(|a| a["name"] == "codex").expect("codex");
        let servers = codex["mcp_servers"].as_array().expect("mcp_servers array");
        // Two declared servers (toggleable) + 4 built-in plugins (non-toggleable).
        let declared: Vec<_> = servers
            .iter()
            .filter(|s| s["toggleable"] == json!(true))
            .map(|s| (s["name"].as_str().unwrap(), s["enabled"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            declared,
            vec![("chrome-devtools", true), ("xcodebuildmcp", false)],
            "allowlist opts chrome-devtools in, disables xcodebuildmcp"
        );
        let plugins: Vec<_> = servers
            .iter()
            .filter(|s| s["toggleable"] == json!(false))
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            plugins,
            crate::delegate_codex_mcp::NON_TOGGLEABLE_PLUGIN_SERVERS.to_vec()
        );
        // The other agents never carry the codex-specific view.
        for other in agents.iter().filter(|a| a["name"] != "codex") {
            assert!(other.get("mcp_servers").is_none(), "{other}");
        }
    }

    #[test]
    fn probe_reports_available_for_an_executable_on_path() {
        // Plant an executable in a temp dir, point PATH at it, and confirm the
        // probe resolves it (existence + exec bit) without spawning it.
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("codex");
        std::fs::write(&bin, "#!/bin/sh\necho should-not-run\n").expect("write bin");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let resolved = with_path(dir.path(), || resolve_on_path("codex"));
        #[cfg(unix)]
        assert_eq!(resolved.as_deref(), Some(bin.as_path()));
        #[cfg(not(unix))]
        assert!(resolved.is_some());
    }

    #[test]
    fn probe_reports_unavailable_for_a_missing_binary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = with_path(dir.path(), || resolve_on_path("definitely-not-here-xyz"));
        assert!(resolved.is_none());
    }

    /// Run `f` with `PATH` set to exactly `dir`, restoring the prior value after.
    /// Serialized by a process-global lock so concurrent tests don't clobber each
    /// other's `PATH`.
    fn with_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("PATH");
        // SAFETY: serialized by LOCK; no other thread reads PATH concurrently here.
        unsafe { std::env::set_var("PATH", dir) };
        let out = f();
        // SAFETY: same lock scope; restore the prior value (or clear if unset).
        unsafe {
            match prev {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
        out
    }
}
