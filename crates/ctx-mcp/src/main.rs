//! Stdio JSON-RPC server and small CLI for the context engine.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
#[cfg(feature = "semantic")]
use ctx_core::semantic::SemanticRuntimeConfig;
use ctx_core::{
    FsCatalogProvider, RootPolicy, ScanOptions, WorkspaceRegistry,
    handle_tool_call_json_with_resolver, tool_specs,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

#[derive(Debug, Parser)]
#[command(
    name = "ctx-mcp",
    version,
    about = "Minimal snapshot-centered context engine"
)]
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
    /// Register ctx-mcp as an MCP server in Claude Code and/or Codex.
    Install(InstallArgs),
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
    /// Enable semantic indexing for semantic_search.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-index")]
    semantic_index: bool,
    /// Embedding model name for semantic_search.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-embedding-model")]
    semantic_embedding_model: Option<String>,
    /// Reranker model name for semantic_search.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-reranker-model")]
    semantic_reranker_model: Option<String>,
    /// Model cache directory for semantic_search providers.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-model-cache-dir")]
    semantic_model_cache_dir: Option<PathBuf>,
    /// Persistent semantic index cache directory.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-cache-dir")]
    semantic_cache_dir: Option<PathBuf>,
    /// Disable semantic_search reranking.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-no-rerank")]
    semantic_no_rerank: bool,
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
        CommandKind::Install(args) => install(args),
    }
}

fn scan_options(args: &ServeArgs) -> ScanOptions {
    ScanOptions {
        max_entries: args.max_entries,
        ..ScanOptions::default()
    }
}

#[cfg(feature = "semantic")]
fn semantic_runtime_config(args: &ServeArgs) -> SemanticRuntimeConfig {
    SemanticRuntimeConfig {
        enabled: args.semantic_index,
        embedding_model: args.semantic_embedding_model.clone(),
        reranker_model: args.semantic_reranker_model.clone(),
        model_cache_dir: args.semantic_model_cache_dir.clone(),
        index_cache_dir: args.semantic_cache_dir.clone(),
        rerank: !args.semantic_no_rerank,
        mock: false,
    }
}

fn provider_for_roots(
    roots: Vec<PathBuf>,
    options: ScanOptions,
    args: &ServeArgs,
) -> Result<FsCatalogProvider> {
    let policy = RootPolicy::new(roots).context("invalid root policy")?;
    #[cfg(feature = "semantic")]
    {
        let semantic = semantic_runtime_config(args);
        let semantic_index = semantic
            .build_index_for_roots(policy.roots())
            .context("failed to initialize semantic index")?;
        Ok(FsCatalogProvider::with_semantic_index(
            policy,
            options,
            semantic_index,
        ))
    }
    #[cfg(not(feature = "semantic"))]
    {
        let _ = args;
        Ok(FsCatalogProvider::new(policy, options))
    }
}

fn registry(args: &ServeArgs) -> Result<WorkspaceRegistry> {
    let options = scan_options(args);
    #[cfg(feature = "semantic")]
    let registry: WorkspaceRegistry<FsCatalogProvider> =
        WorkspaceRegistry::with_scan_options_and_semantic(
            options.clone(),
            semantic_runtime_config(args),
        );
    #[cfg(not(feature = "semantic"))]
    let registry: WorkspaceRegistry<FsCatalogProvider> =
        WorkspaceRegistry::with_scan_options(options.clone());
    registry.insert(
        "default",
        std::sync::Arc::new(provider_for_roots(
            args.roots.clone(),
            options.clone(),
            args,
        )?),
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

#[derive(Debug, Args)]
struct InstallArgs {
    /// Workspace root to expose. Repeatable. Defaults to the current directory.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Additional named workspace as name=path. Repeatable.
    #[arg(long = "workspace")]
    workspaces: Vec<WorkspaceArg>,
    /// Name to register the MCP server under.
    #[arg(long, default_value = "context-engine")]
    name: String,
    /// Configure Claude Code only (default: configure both).
    #[arg(long)]
    claude: bool,
    /// Configure Codex only (default: configure both).
    #[arg(long)]
    codex: bool,
    /// Claude Code config scope: local, user, or project.
    #[arg(long, default_value = "local")]
    scope: String,
    /// Override the ctx-mcp executable path written into the config.
    #[arg(long)]
    command: Option<PathBuf>,
    /// Print the commands that would run without executing them.
    #[arg(long)]
    dry_run: bool,
}

/// Decide which tools to configure: both unless exactly one flag is set.
fn select_targets(claude: bool, codex: bool) -> (bool, bool) {
    if claude == codex {
        (true, true)
    } else {
        (claude, codex)
    }
}

fn build_serve_args(roots: &[String], workspaces: &[(String, String)]) -> Vec<String> {
    let mut args = vec!["serve".to_string()];
    for root in roots {
        args.push("--root".to_string());
        args.push(root.clone());
    }
    for (name, path) in workspaces {
        args.push("--workspace".to_string());
        args.push(format!("{name}={path}"));
    }
    args
}

/// First match for `tool` on PATH, if any.
fn which(tool: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// The path to write into client configs. Prefers Homebrew's stable `bin`
/// symlink over the versioned Cellar path so configs survive `brew upgrade`.
fn resolve_command(override_cmd: Option<PathBuf>) -> Result<String> {
    if let Some(cmd) = override_cmd {
        return Ok(cmd.to_string_lossy().into_owned());
    }
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let exe_str = exe.to_string_lossy();
    if let Some(idx) = exe_str.find("/Cellar/ctx-mcp/") {
        let linked = PathBuf::from(format!("{}/bin/ctx-mcp", &exe_str[..idx]));
        if linked.exists() {
            return Ok(linked.to_string_lossy().into_owned());
        }
    }
    Ok(exe_str.into_owned())
}

fn canonical(path: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("path does not exist: {}", path.display()))?;
    Ok(abs.to_string_lossy().into_owned())
}

fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '=' | ':' | ',' | '+')
        });
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn print_command(program: &str, args: &[String]) {
    let mut line = String::from(program);
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    println!("{line}");
}

fn run_tool(program: &str, args: &[String]) -> Result<bool> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    Ok(status.success())
}

fn install(args: InstallArgs) -> Result<()> {
    let command = resolve_command(args.command.clone())?;

    let mut roots: Vec<String> = Vec::new();
    if args.roots.is_empty() {
        roots.push(canonical(
            &std::env::current_dir().context("failed to read current directory")?,
        )?);
    } else {
        for root in &args.roots {
            roots.push(canonical(root)?);
        }
    }
    let workspaces: Vec<(String, String)> = args
        .workspaces
        .iter()
        .map(|w| Ok((w.name.clone(), canonical(&w.path)?)))
        .collect::<Result<_>>()?;

    let serve_args = build_serve_args(&roots, &workspaces);
    let (do_claude, do_codex) = select_targets(args.claude, args.codex);

    println!("ctx-mcp: {command}");
    println!("roots:   {}", roots.join(", "));
    if !workspaces.is_empty() {
        let rendered: Vec<String> = workspaces.iter().map(|(n, p)| format!("{n}={p}")).collect();
        println!("workspaces: {}", rendered.join(", "));
    }
    println!();

    let mut configured = false;
    if do_claude {
        configured |=
            configure_claude(&args.name, &args.scope, &command, &serve_args, args.dry_run)?;
    }
    if do_codex {
        configured |= configure_codex(&args.name, &command, &serve_args, args.dry_run)?;
    }

    if configured && !args.dry_run {
        println!();
        println!(
            "Done. Restart Claude Code / Codex to pick up '{}'.",
            args.name
        );
    }
    Ok(())
}

fn configure_claude(
    name: &str,
    scope: &str,
    command: &str,
    serve_args: &[String],
    dry_run: bool,
) -> Result<bool> {
    let mut add = vec![
        "mcp".to_string(),
        "add".to_string(),
        "-s".to_string(),
        scope.to_string(),
        name.to_string(),
        "--".to_string(),
        command.to_string(),
    ];
    add.extend(serve_args.iter().cloned());

    if which("claude").is_none() {
        println!("\u{2022} Claude Code: `claude` CLI not found \u{2014} add it manually:");
        print!("    ");
        print_command("claude", &add);
        return Ok(false);
    }
    if dry_run {
        print_command("claude", &add);
        return Ok(true);
    }
    // idempotent: drop any existing entry in this scope first
    let _ = Command::new("claude")
        .args(["mcp", "remove", "-s", scope, name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if run_tool("claude", &add)? {
        println!("\u{2713} Claude Code: registered '{name}' (scope: {scope})");
        Ok(true)
    } else {
        bail!("claude mcp add failed");
    }
}

fn configure_codex(
    name: &str,
    command: &str,
    serve_args: &[String],
    dry_run: bool,
) -> Result<bool> {
    let mut add = vec![
        "mcp".to_string(),
        "add".to_string(),
        name.to_string(),
        "--".to_string(),
        command.to_string(),
    ];
    add.extend(serve_args.iter().cloned());

    if which("codex").is_none() {
        println!("\u{2022} Codex: `codex` CLI not found \u{2014} add it manually:");
        print!("    ");
        print_command("codex", &add);
        return Ok(false);
    }
    if dry_run {
        print_command("codex", &add);
        return Ok(true);
    }
    let _ = Command::new("codex")
        .args(["mcp", "remove", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if run_tool("codex", &add)? {
        println!("\u{2713} Codex: registered '{name}'");
        Ok(true)
    } else {
        bail!("codex mcp add failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn select_targets_defaults_to_both() {
        assert_eq!(select_targets(false, false), (true, true));
        assert_eq!(select_targets(true, true), (true, true));
        assert_eq!(select_targets(true, false), (true, false));
        assert_eq!(select_targets(false, true), (false, true));
    }

    #[test]
    fn build_serve_args_includes_roots_and_workspaces() {
        let args = build_serve_args(
            &["/a".to_string(), "/b".to_string()],
            &[("named".to_string(), "/c".to_string())],
        );
        assert_eq!(
            args,
            vec![
                "serve",
                "--root",
                "/a",
                "--root",
                "/b",
                "--workspace",
                "named=/c"
            ]
        );
    }

    #[test]
    fn shell_quote_quotes_unsafe_values() {
        assert_eq!(shell_quote("/abs/path"), "/abs/path");
        assert_eq!(shell_quote("with space"), "'with space'");
        assert_eq!(shell_quote(""), "''");
    }

    fn args_with(roots: Vec<PathBuf>, workspaces: Vec<WorkspaceArg>) -> ServeArgs {
        ServeArgs {
            roots,
            workspaces,
            max_entries: 10_000,
            #[cfg(feature = "semantic")]
            semantic_index: false,
            #[cfg(feature = "semantic")]
            semantic_embedding_model: None,
            #[cfg(feature = "semantic")]
            semantic_reranker_model: None,
            #[cfg(feature = "semantic")]
            semantic_model_cache_dir: None,
            #[cfg(feature = "semantic")]
            semantic_cache_dir: None,
            #[cfg(feature = "semantic")]
            semantic_no_rerank: false,
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

    #[cfg(feature = "semantic")]
    #[test]
    fn semantic_flags_flow_to_registry_and_named_workspaces() {
        let named_dir = tempfile::tempdir().expect("named tempdir");
        fs::write(
            named_dir.path().join("config.rs"),
            "pub fn validate_config() {}\n",
        )
        .expect("named write");
        let mut args = args_with(
            Vec::new(),
            vec![WorkspaceArg {
                name: "named".to_string(),
                path: named_dir.path().to_path_buf(),
            }],
        );
        args.semantic_index = true;
        args.semantic_embedding_model = Some("mock".to_string());
        args.semantic_reranker_model = Some("mock".to_string());

        let registry = registry(&args).expect("registry");
        let mut initialized = true;
        let response = handle_message(
            &registry,
            &mut initialized,
            RpcMessage {
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "semantic_search",
                    "arguments": {
                        "workspace": "named",
                        "query": "config validation",
                        "max_results": 1
                    }
                }),
            },
        )
        .expect("response");
        assert_eq!(
            response["result"]["structuredContent"]["results"][0]["path"],
            Value::String("config.rs".to_string())
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
