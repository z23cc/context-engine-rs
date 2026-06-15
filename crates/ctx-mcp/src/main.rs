//! Stdio JSON-RPC server and small CLI for the context engine.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use ctx_core::{
    FsCatalogProvider, RootPolicy, ScanOptions, WorkspaceRegistry,
    handle_tool_call_json_with_resolver, tool_specs,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::Command,
    str::FromStr,
};

#[derive(Debug, Parser)]
#[command(name = "ctx-mcp", version, about = "Minimal snapshot-centered context engine")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Run a synchronous JSON-RPC stdio MCP-like server.
    Serve(ServeArgs),
    /// Print local toolchain diagnostics.
    Doctor,
    /// Inspect configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Args, Clone)]
struct ServeArgs {
    /// Allowed root for the default workspace. Repeatable. If absent, default operations fail closed.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Additional named workspace as name=path. Repeat to add workspaces or multiple roots per name.
    #[arg(long = "workspace")]
    workspaces: Vec<WorkspaceArg>,
    /// Maximum catalog entries per workspace.
    #[arg(long, default_value_t = 10_000)]
    max_entries: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceArg {
    name: String,
    path: PathBuf,
}

impl FromStr for WorkspaceArg {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (name, path) = value
            .split_once('=')
            .ok_or_else(|| "workspace must be name=path".to_string())?;
        if name.is_empty() {
            return Err("workspace name must not be empty".to_string());
        }
        if path.is_empty() {
            return Err("workspace path must not be empty".to_string());
        }
        Ok(Self {
            name: name.to_string(),
            path: PathBuf::from(path),
        })
    }
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Show canonical roots that would be allowed.
    Roots(ServeArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CommandKind::Serve(args) => serve(args),
        CommandKind::Doctor => doctor(),
        CommandKind::Config(args) => match args.command {
            ConfigCommand::Roots(serve_args) => config_roots(serve_args),
        },
    }
}

fn scan_options(args: &ServeArgs) -> ScanOptions {
    ScanOptions {
        max_entries: args.max_entries,
        ..ScanOptions::default()
    }
}

fn provider_for_roots(roots: Vec<PathBuf>, options: ScanOptions) -> Result<FsCatalogProvider> {
    let policy = RootPolicy::new(roots).context("invalid root policy")?;
    Ok(FsCatalogProvider::new(policy, options))
}

fn registry(args: &ServeArgs) -> Result<WorkspaceRegistry> {
    let options = scan_options(args);
    let registry: WorkspaceRegistry<FsCatalogProvider> =
        WorkspaceRegistry::with_scan_options(options.clone());
    registry.insert(
        "default",
        std::sync::Arc::new(provider_for_roots(args.roots.clone(), options.clone())?),
    );

    let mut grouped: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for workspace in &args.workspaces {
        if workspace.name == "default" {
            bail!("--workspace default=... conflicts with --root default workspace");
        }
        grouped
            .entry(workspace.name.clone())
            .or_default()
            .push(workspace.path.clone());
    }
    for (name, roots) in grouped {
        registry.add_workspace(name, roots)?;
    }
    Ok(registry)
}

fn serve(args: ServeArgs) -> Result<()> {
    let registry = registry(&args)?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut initialized = false;

    for line in stdin.lock().lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: RpcMessage = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                write_response(
                    &mut stdout,
                    jsonrpc_error(Value::Null, -32700, err.to_string()),
                )?;
                continue;
            }
        };

        let maybe_response = handle_message(&registry, &mut initialized, request);
        if let Some(response) = maybe_response {
            write_response(&mut stdout, response)?;
        }
    }
    Ok(())
}

fn write_response(mut out: impl Write, value: Value) -> Result<()> {
    serde_json::to_writer(&mut out, &value).context("failed to encode response")?;
    writeln!(out).context("failed to write response")?;
    out.flush().context("failed to flush response")
}

#[derive(Debug, Deserialize)]
struct RpcMessage {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn handle_message(
    registry: &WorkspaceRegistry,
    initialized: &mut bool,
    message: RpcMessage,
) -> Option<Value> {
    let id = message.id.clone().unwrap_or(Value::Null);
    match message.method.as_str() {
        "initialize" => Some(jsonrpc_result(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "ctx-mcp", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": { "listChanged": false } }
            }),
        )),
        "notifications/initialized" => {
            *initialized = true;
            None
        }
        _ if !*initialized => Some(jsonrpc_error(id, -32002, "not initialized")),
        "tools/list" => Some(jsonrpc_result(id, json!({ "tools": tool_specs() }))),
        "tools/call" => Some(
            match serde_json::to_string(&message.params)
                .map_err(|err| err.to_string())
                .and_then(|request| {
                    handle_tool_call_json_with_resolver(registry, &request)
                        .map_err(|err| err.to_string())
                })
                .and_then(|response| serde_json::from_str(&response).map_err(|err| err.to_string()))
            {
                Ok(value) => jsonrpc_result(id, value),
                Err(err) => jsonrpc_error(id, -32000, err),
            },
        ),
        _ => Some(jsonrpc_error(id, -32601, "method not found")),
    }
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn jsonrpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

fn doctor() -> Result<()> {
    println!("ctx-mcp doctor");
    print_command_version("rustc", ["--version"])?;
    print_command_version("cargo", ["--version"])?;
    println!("default features: codemap disabled (no C compiler required)");
    println!("status: ok");
    Ok(())
}

fn print_command_version<const N: usize>(cmd: &str, args: [&str; N]) -> Result<()> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("run {cmd}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stdout.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    println!("{cmd}: {text}");
    Ok(())
}

fn config_roots(args: ServeArgs) -> Result<()> {
    let policy = RootPolicy::new(args.roots).context("invalid root policy")?;
    if policy.roots().is_empty() {
        println!("roots: []");
        println!("fail_closed: true");
        return Ok(());
    }
    for root in policy.roots() {
        println!("{}\t{}", root.id, root.path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn args_with(roots: Vec<PathBuf>, workspaces: Vec<WorkspaceArg>) -> ServeArgs {
        ServeArgs {
            roots,
            workspaces,
            max_entries: 10_000,
        }
    }

    #[test]
    fn requires_initialized_after_initialize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
        registry
            .add_workspace("default", vec![dir.path().to_path_buf()])
            .expect("workspace");
        let mut initialized = false;
        let response = handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(1)),
                method: "tools/list".to_string(),
                params: json!({}),
            },
        )
        .expect("response");
        assert_eq!(response["error"]["message"], "not initialized");
    }

    #[test]
    fn serve_args_build_default_and_named_workspaces() {
        let default_dir = tempfile::tempdir().expect("default tempdir");
        let named_dir = tempfile::tempdir().expect("named tempdir");
        fs::write(
            default_dir.path().join("default.txt"),
            "alpha
",
        )
        .expect("default write");
        fs::write(
            named_dir.path().join("named.txt"),
            "beta
",
        )
        .expect("named write");

        let registry = registry(&args_with(
            vec![default_dir.path().to_path_buf()],
            vec![WorkspaceArg {
                name: "named".to_string(),
                path: named_dir.path().to_path_buf(),
            }],
        ))
        .expect("registry");
        assert_eq!(registry.len(), 2);

        let mut initialized = true;
        let response = handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "file_search",
                    "arguments": {
                        "workspace": "named",
                        "pattern": "beta",
                        "mode": "content"
                    }
                }),
            },
        )
        .expect("response");
        assert_eq!(
            response["result"]["structuredContent"]["content_matches"][0]["path"],
            Value::String("named.txt".to_string())
        );
    }

    #[test]
    fn manage_workspaces_add_remove_affects_later_routing() {
        let default_dir = tempfile::tempdir().expect("default tempdir");
        let dynamic_dir = tempfile::tempdir().expect("dynamic tempdir");
        fs::write(
            default_dir.path().join("default.txt"),
            "alpha
",
        )
        .expect("default write");
        fs::write(
            dynamic_dir.path().join("dynamic.txt"),
            "dynamic
",
        )
        .expect("dynamic write");

        let registry = registry(&args_with(
            vec![default_dir.path().to_path_buf()],
            Vec::new(),
        ))
        .expect("registry");
        let mut initialized = true;

        let add = handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "manage_workspaces",
                    "arguments": {
                        "op": "add",
                        "name": "dynamic",
                        "roots": [dynamic_dir.path()]
                    }
                }),
            },
        )
        .expect("add response");
        assert_eq!(
            add["result"]["structuredContent"]["workspaces"][0]["name"],
            Value::String("dynamic".to_string())
        );

        let search = handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(2)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "file_search",
                    "arguments": {
                        "workspace": "dynamic",
                        "pattern": "dynamic",
                        "mode": "content"
                    }
                }),
            },
        )
        .expect("search response");
        assert_eq!(
            search["result"]["structuredContent"]["content_matches"][0]["path"],
            Value::String("dynamic.txt".to_string())
        );

        handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(3)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "manage_workspaces",
                    "arguments": { "op": "remove", "name": "dynamic" }
                }),
            },
        )
        .expect("remove response");

        let missing = handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(4)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "file_search",
                    "arguments": { "workspace": "dynamic", "pattern": "dynamic" }
                }),
            },
        )
        .expect("missing response");
        assert!(
            missing["error"]["message"]
                .as_str()
                .expect("error")
                .contains("unknown workspace: dynamic")
        );
    }
}
