//! Stdio JSON-RPC server and small CLI for the context engine.

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
#[cfg(feature = "semantic")]
use ctx_core::semantic::{SemanticIndexScope, SemanticRuntimeConfig};
use ctx_core::{
    FsCatalogProvider, RootPolicy, ScanOptions, WorkspaceRegistry,
    handle_tool_call_json_with_resolver, tool_specs as core_tool_specs,
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

mod auth;
mod cache;
mod install;
mod xai;

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
    /// Warm the current project's semantic index cache.
    Warm(ServeArgs),
    /// Manage xAI OAuth credentials.
    Auth(auth::AuthArgs),
    /// Manage local caches.
    Cache(CacheArgs),
    /// Register ctx-mcp as an MCP server in Claude Code and/or Codex.
    Install(install::InstallArgs),
}

#[derive(Debug, Args, Clone)]
struct ServeArgs {
    /// Allowed root for the default workspace. Repeatable. `serve` fails closed when absent; `warm`/`cache purge` default to the current directory when no roots or workspaces are supplied.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Additional named workspace as name=path. Repeat to add workspaces or multiple roots per name.
    #[arg(long = "workspace")]
    workspaces: Vec<WorkspaceArg>,
    /// Maximum catalog entries per workspace.
    #[arg(long, default_value_t = 100_000)]
    max_entries: usize,
    /// Disable the built-in semantic_search index (on by default).
    #[cfg(feature = "semantic")]
    #[arg(long = "no-semantic")]
    no_semantic: bool,
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
    /// Enable semantic_search reranking (off by default). On local code corpora
    /// the available cross-encoder rerankers do not beat the fused BM25+dense
    /// ranking and add 15-20x query latency — see crates/ctx-core/tests/eval.rs.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-rerank")]
    semantic_rerank: bool,
    /// Restrict semantic indexing to paths matching this glob. Repeatable.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-include")]
    semantic_include: Vec<String>,
    /// Exclude paths from semantic indexing with this glob. Repeatable.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-exclude")]
    semantic_exclude: Vec<String>,
    /// Restrict semantic indexing to this extension (dot optional). Repeatable.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-extension")]
    semantic_extensions: Vec<String>,
    /// Do not apply the default semantic excludes for tests/docs/vendor/build/generated files.
    #[cfg(feature = "semantic")]
    #[arg(long = "semantic-no-default-excludes")]
    semantic_no_default_excludes: bool,
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

#[derive(Debug, Args)]
struct CacheArgs {
    #[command(subcommand)]
    command: CacheCommand,
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Delete the current project's semantic index cache.
    Purge(ServeArgs),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CommandKind::Serve(args) => serve(args),
        CommandKind::Doctor => doctor(),
        CommandKind::Config(args) => match args.command {
            ConfigCommand::Roots(serve_args) => config_roots(serve_args),
        },
        CommandKind::Warm(args) => cache::warm(args),
        CommandKind::Auth(args) => auth::run(args),
        CommandKind::Cache(args) => match args.command {
            CacheCommand::Purge(serve_args) => cache::purge(serve_args),
        },
        CommandKind::Install(args) => install::install(args),
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
        enabled: !args.no_semantic,
        embedding_model: args.semantic_embedding_model.clone(),
        reranker_model: args.semantic_reranker_model.clone(),
        model_cache_dir: args.semantic_model_cache_dir.clone(),
        index_cache_dir: args.semantic_cache_dir.clone(),
        rerank: args.semantic_rerank,
        mock: false,
        scope: SemanticIndexScope {
            extensions: args.semantic_extensions.clone(),
            include: args.semantic_include.clone(),
            exclude: args.semantic_exclude.clone(),
            use_default_excludes: !args.semantic_no_default_excludes,
        },
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
        "tools/call" => Some(match handle_tool_call(registry, &message.params) {
            Ok(value) => jsonrpc_result(id, value),
            Err(err) => jsonrpc_error(id, -32000, err),
        }),
        _ => Some(jsonrpc_error(id, -32601, "method not found")),
    }
}

fn tool_specs() -> Value {
    let mut tools = core_tool_specs().as_array().cloned().unwrap_or_default();
    tools.extend(xai::tool_specs());
    Value::Array(tools)
}

fn handle_tool_call(
    registry: &WorkspaceRegistry,
    params: &Value,
) -> std::result::Result<Value, String> {
    match xai::handle_tool_call(registry, params) {
        Ok(Some(value)) => return Ok(value),
        Ok(None) => {}
        Err(err) => return Err(err.to_string()),
    }

    serde_json::to_string(params)
        .map_err(|err| err.to_string())
        .and_then(|request| {
            handle_tool_call_json_with_resolver(registry, &request).map_err(|err| err.to_string())
        })
        .and_then(|response| serde_json::from_str(&response).map_err(|err| err.to_string()))
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

    #[test]
    fn select_targets_defaults_to_both() {
        assert_eq!(install::select_targets(false, false), (true, true));
        assert_eq!(install::select_targets(true, true), (true, true));
        assert_eq!(install::select_targets(true, false), (true, false));
        assert_eq!(install::select_targets(false, true), (false, true));
    }

    #[test]
    fn build_serve_args_includes_roots_and_workspaces() {
        let args = install::build_serve_args(
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
        assert_eq!(install::shell_quote("/abs/path"), "/abs/path");
        assert_eq!(install::shell_quote("with space"), "'with space'");
        assert_eq!(install::shell_quote(""), "''");
    }

    #[test]
    fn cli_parses_warm_cache_and_auth() {
        let warm = Cli::try_parse_from(["ctx-mcp", "warm"]).expect("warm parse");
        assert!(matches!(warm.command, CommandKind::Warm(_)));
        let purge = Cli::try_parse_from(["ctx-mcp", "cache", "purge"]).expect("purge parse");
        assert!(matches!(purge.command, CommandKind::Cache(_)));
        let login =
            Cli::try_parse_from(["ctx-mcp", "auth", "login", "xai"]).expect("auth login parse");
        assert!(matches!(login.command, CommandKind::Auth(_)));
        let status = Cli::try_parse_from(["ctx-mcp", "auth", "status"]).expect("status parse");
        assert!(matches!(status.command, CommandKind::Auth(_)));
        let logout = Cli::try_parse_from(["ctx-mcp", "auth", "logout"]).expect("logout parse");
        assert!(matches!(logout.command, CommandKind::Auth(_)));
    }

    #[test]
    fn tool_specs_include_core_and_xai_tools() {
        let specs = tool_specs();
        let names: Vec<_> = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"file_search"));
        assert!(names.contains(&"xai_responses"));
        assert!(names.contains(&"xai_x_search"));
    }

    fn args_with(roots: Vec<PathBuf>, workspaces: Vec<WorkspaceArg>) -> ServeArgs {
        ServeArgs {
            roots,
            workspaces,
            max_entries: 100_000,
            #[cfg(feature = "semantic")]
            no_semantic: true,
            #[cfg(feature = "semantic")]
            semantic_embedding_model: None,
            #[cfg(feature = "semantic")]
            semantic_reranker_model: None,
            #[cfg(feature = "semantic")]
            semantic_model_cache_dir: None,
            #[cfg(feature = "semantic")]
            semantic_cache_dir: None,
            #[cfg(feature = "semantic")]
            semantic_rerank: false,
            #[cfg(feature = "semantic")]
            semantic_include: Vec::new(),
            #[cfg(feature = "semantic")]
            semantic_exclude: Vec::new(),
            #[cfg(feature = "semantic")]
            semantic_extensions: Vec::new(),
            #[cfg(feature = "semantic")]
            semantic_no_default_excludes: false,
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
    fn warm_and_cache_purge_use_project_cache() {
        let root = tempfile::tempdir().expect("root tempdir");
        let cache = tempfile::tempdir().expect("cache tempdir");
        fs::create_dir_all(root.path().join("src")).expect("src dir");
        fs::write(root.path().join("src/lib.rs"), "pub fn parse_config() {}\n")
            .expect("source write");
        let mut args = args_with(vec![root.path().to_path_buf()], Vec::new());
        args.no_semantic = false;
        args.semantic_embedding_model = Some("mock".to_string());
        args.semantic_reranker_model = Some("mock".to_string());
        args.semantic_cache_dir = Some(cache.path().to_path_buf());

        cache::warm_semantic(args.clone()).expect("warm");
        assert!(fs::read_dir(cache.path()).expect("cache dir").count() > 0);
        cache::purge_semantic_cache(args).expect("purge");
        assert_eq!(fs::read_dir(cache.path()).expect("cache dir").count(), 0);
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
        args.no_semantic = false;
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
