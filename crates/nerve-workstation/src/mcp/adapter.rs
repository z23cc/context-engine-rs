//! MCP-client tool adapter: exposes external MCP servers' tools through the
//! `RuntimeToolAdapter` seam (per the architecture north star — tools-as-plugins,
//! no recompile). Tools are namespaced `mcp__<server>__<tool>` so two servers
//! can't shadow each other or a core tool. A namespace clash with an already
//! known tool is a **hard load error** (via [`RuntimeToolAdapter::owns`]), never
//! silent shadowing — a connect/list *failure* is still tolerated and skipped.

use super::client::McpStdioClient;
use super::config::McpServerConfig;
use anyhow::{Result, bail};
use nerve_fs::FsWorkspaceRegistry;
use nerve_runtime::{RuntimeError, RuntimeToolAdapter};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

pub(crate) struct McpClientToolAdapter {
    /// Connected clients keyed by server name. A per-server `Mutex` serializes
    /// that server's requests while allowing concurrency across servers.
    clients: HashMap<String, Mutex<McpStdioClient>>,
    /// Namespaced tool specs advertised to the runtime.
    specs: Vec<Value>,
    /// Namespaced names this adapter claims, for `owns` / collision detection.
    owned: HashSet<String>,
}

impl McpClientToolAdapter {
    /// Connect to every configured server, list its tools, and build the
    /// namespaced spec catalog.
    ///
    /// `reserved` is the set of tool names already claimed elsewhere (core tools
    /// plus any earlier adapters). Collision policy:
    ///   * A *connect* or *tools/list* failure is tolerated — logged and skipped,
    ///     so one bad server never breaks startup.
    ///   * A namespaced tool whose name collides with a `reserved` name, or a
    ///     duplicate server name in the config, is a **hard error**: we refuse to
    ///     load rather than silently shadow. (The `mcp__<server>__` prefix means
    ///     two distinct servers can never collide with each other, but a server
    ///     repeated in the config, or a core tool already named `mcp__…`, can.)
    pub(crate) fn connect(
        configs: Vec<McpServerConfig>,
        reserved: &HashSet<String>,
    ) -> Result<Self> {
        let mut clients = HashMap::new();
        let mut specs = Vec::new();
        let mut owned = HashSet::new();
        for config in configs {
            if clients.contains_key(&config.name) {
                bail!("[mcp] duplicate server name '{}' in config", config.name);
            }
            let mut client = match McpStdioClient::connect(&config) {
                Ok(client) => client,
                Err(err) => {
                    eprintln!("[mcp] '{}' connect failed: {err}", config.name);
                    continue;
                }
            };
            let tools = match client.list_tools() {
                Ok(tools) => tools,
                Err(err) => {
                    eprintln!("[mcp] '{}' tools/list failed: {err}", config.name);
                    continue;
                }
            };
            claim_namespaced_tools(&config.name, &tools, reserved, &mut owned, &mut specs)?;
            clients.insert(config.name.clone(), Mutex::new(client));
        }
        Ok(Self {
            clients,
            specs,
            owned,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }
}

impl RuntimeToolAdapter<FsWorkspaceRegistry> for McpClientToolAdapter {
    fn tool_specs(&self) -> Vec<Value> {
        self.specs.clone()
    }

    fn owns(&self, name: &str) -> bool {
        self.owned.contains(name)
    }

    fn handle_tool_call(
        &self,
        _resolver: &FsWorkspaceRegistry,
        params: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some((server, tool)) = parse_namespaced(name) else {
            return Ok(None);
        };
        let Some(client) = self.clients.get(server) else {
            return Ok(None);
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut client = client
            .lock()
            .map_err(|_| RuntimeError::adapter("mcp client lock poisoned"))?;
        let result = client
            .call_tool(tool, &arguments)
            .map_err(|err| RuntimeError::adapter(err.to_string()))?;
        Ok(Some(result))
    }
}

/// Rewrite an MCP tool spec's name to `mcp__<server>__<tool>`, keeping its
/// description and input schema intact.
fn namespaced_spec(server: &str, tool: &Value) -> Option<Value> {
    let name = tool.get("name").and_then(Value::as_str)?;
    let mut spec = tool.clone();
    spec.as_object_mut()?
        .insert("name".to_string(), json!(format!("mcp__{server}__{name}")));
    Some(spec)
}

/// The `name` field of a tool spec, if present.
fn spec_name(spec: &Value) -> Option<String> {
    spec.get("name").and_then(Value::as_str).map(str::to_string)
}

/// Namespace one server's tools and add them to `owned`/`specs`, failing on the
/// first collision with `reserved` (core/earlier tools) or `owned` (a name this
/// server already produced). Pure over its inputs, so the collision policy is
/// unit-testable without spawning an MCP server.
fn claim_namespaced_tools(
    server: &str,
    tools: &[Value],
    reserved: &HashSet<String>,
    owned: &mut HashSet<String>,
    specs: &mut Vec<Value>,
) -> Result<()> {
    for tool in tools {
        let Some(spec) = namespaced_spec(server, tool) else {
            continue;
        };
        let name = spec_name(&spec).expect("namespaced_spec always sets a name");
        if reserved.contains(&name) || !owned.insert(name.clone()) {
            bail!(
                "[mcp] tool name collision: '{name}' from server '{server}' shadows an existing \
                 tool — refusing to load (rename the server or disable the tool)"
            );
        }
        specs.push(spec);
    }
    Ok(())
}

/// Parse `mcp__<server>__<tool>` back into `(server, tool)`. Server names never
/// contain `__` (enforced at config load), so the first `__` split is correct.
fn parse_namespaced(name: &str) -> Option<(&str, &str)> {
    name.strip_prefix("mcp__")?.split_once("__")
}

#[cfg(test)]
mod tests {
    use super::{claim_namespaced_tools, namespaced_spec, parse_namespaced};
    use serde_json::json;
    use std::collections::HashSet;

    fn tool(name: &str) -> serde_json::Value {
        json!({ "name": name, "inputSchema": { "type": "object" } })
    }

    #[test]
    fn claim_namespaces_tools_and_records_ownership() {
        let reserved = HashSet::new();
        let mut owned = HashSet::new();
        let mut specs = Vec::new();
        claim_namespaced_tools(
            "fs",
            &[tool("read"), tool("write")],
            &reserved,
            &mut owned,
            &mut specs,
        )
        .expect("no collision");
        assert_eq!(specs.len(), 2);
        assert!(owned.contains("mcp__fs__read"));
        assert!(owned.contains("mcp__fs__write"));
    }

    #[test]
    fn claim_rejects_collision_with_reserved_core_name() {
        // A core tool already named `mcp__fs__read` (contrived, but the guard must
        // catch it) blocks the load rather than silently shadowing.
        let reserved: HashSet<String> = ["mcp__fs__read".to_string()].into_iter().collect();
        let mut owned = HashSet::new();
        let mut specs = Vec::new();
        let err = claim_namespaced_tools("fs", &[tool("read")], &reserved, &mut owned, &mut specs)
            .expect_err("collision must be a hard error");
        assert!(err.to_string().contains("collision"), "{err}");
        assert!(specs.is_empty());
    }

    #[test]
    fn claim_rejects_duplicate_tool_within_a_server() {
        let reserved = HashSet::new();
        let mut owned = HashSet::new();
        let mut specs = Vec::new();
        let err = claim_namespaced_tools(
            "fs",
            &[tool("read"), tool("read")],
            &reserved,
            &mut owned,
            &mut specs,
        )
        .expect_err("duplicate name must be a hard error");
        assert!(err.to_string().contains("collision"), "{err}");
    }

    #[test]
    fn distinct_servers_never_collide_on_the_same_tool() {
        let reserved = HashSet::new();
        let mut owned = HashSet::new();
        let mut specs = Vec::new();
        // Same bare tool name `read` from two servers namespaces apart cleanly.
        claim_namespaced_tools("a", &[tool("read")], &reserved, &mut owned, &mut specs)
            .expect("server a ok");
        claim_namespaced_tools("b", &[tool("read")], &reserved, &mut owned, &mut specs)
            .expect("server b ok — different namespace");
        assert!(owned.contains("mcp__a__read"));
        assert!(owned.contains("mcp__b__read"));
    }

    #[test]
    fn namespaces_tool_name_and_keeps_schema() {
        let spec = namespaced_spec(
            "fs",
            &json!({ "name": "read_file", "description": "d", "inputSchema": { "type": "object" } }),
        )
        .expect("spec");
        assert_eq!(spec["name"], "mcp__fs__read_file");
        assert_eq!(spec["description"], "d");
        assert_eq!(spec["inputSchema"]["type"], "object");
    }

    #[test]
    fn parses_namespaced_names() {
        assert_eq!(
            parse_namespaced("mcp__fs__read_file"),
            Some(("fs", "read_file"))
        );
        // A tool whose own name contains `__` round-trips (server has no `__`).
        assert_eq!(
            parse_namespaced("mcp__git__log__oneline"),
            Some(("git", "log__oneline"))
        );
        assert_eq!(parse_namespaced("file_search"), None);
        assert_eq!(parse_namespaced("mcp__lonely"), None);
    }
}
