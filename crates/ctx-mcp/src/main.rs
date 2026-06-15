//! Stdio JSON-RPC server and small CLI for the context engine.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use ctx_core::{FsCatalogProvider, RootPolicy, ScanOptions, handle_tool_call, tool_specs};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    process::Command,
};

#[derive(Debug, Parser)]
#[command(name = "ctx-mcp", about = "Minimal snapshot-centered context engine")]
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
    /// Allowed root. Repeatable. If absent, operations fail closed.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Maximum catalog entries.
    #[arg(long, default_value_t = 10_000)]
    max_entries: usize,
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

fn provider(args: &ServeArgs) -> Result<FsCatalogProvider> {
    let policy = RootPolicy::new(args.roots.clone()).context("invalid root policy")?;
    Ok(FsCatalogProvider::new(
        policy,
        ScanOptions {
            max_entries: args.max_entries,
            ..ScanOptions::default()
        },
    ))
}

fn serve(args: ServeArgs) -> Result<()> {
    let provider = provider(&args)?;
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

        let maybe_response = handle_message(&provider, &mut initialized, request);
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
    provider: &FsCatalogProvider,
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
        "tools/call" => Some(match handle_tool_call(provider, &message.params) {
            Ok(value) => jsonrpc_result(id, value),
            Err(err) => jsonrpc_error(id, -32000, err.to_string()),
        }),
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

    #[test]
    fn requires_initialized_after_initialize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let mut initialized = false;
        let response = handle_message(
            &provider,
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
}
