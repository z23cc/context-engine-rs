use crate::rpc::{RpcMessage, jsonrpc_error, jsonrpc_result, write_response};
use crate::{tools, workspace};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{self, BufRead};

pub(crate) fn serve(args: workspace::ServeArgs) -> Result<()> {
    let runtime = crate::mcp::attach(tools::runtime(workspace::registry(&args)?), &args)?;
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

        let maybe_response = handle_message(&runtime, &mut initialized, request);
        if let Some(response) = maybe_response {
            write_response(&mut stdout, response)?;
        }
    }
    Ok(())
}

pub(crate) fn handle_message(
    runtime: &tools::NerveRuntime,
    initialized: &mut bool,
    message: RpcMessage,
) -> Option<Value> {
    let id = message.id.clone().unwrap_or(Value::Null);
    match message.method.as_str() {
        "initialize" => Some(jsonrpc_result(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": { "name": "nerve", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "tools": { "listChanged": false } }
            }),
        )),
        "notifications/initialized" => {
            *initialized = true;
            None
        }
        _ if !*initialized => Some(jsonrpc_error(id, -32002, "not initialized")),
        "tools/list" => Some(jsonrpc_result(id, json!({ "tools": runtime.tool_specs() }))),
        "tools/call" => Some(match runtime.handle_tool_call(&message.params) {
            Ok(value) => jsonrpc_result(id, value),
            Err(err) => jsonrpc_error(id, -32000, err.to_string()),
        }),
        _ => Some(jsonrpc_error(id, -32601, "method not found")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{WorkspaceArg, args_with, registry};
    use nerve_fs::FsWorkspaceRegistry;
    use std::fs;

    #[test]
    fn requires_initialized_after_initialize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry: FsWorkspaceRegistry = FsWorkspaceRegistry::new();
        registry
            .add_workspace("default", vec![dir.path().to_path_buf()])
            .expect("workspace");
        let runtime = tools::runtime(registry);
        let mut initialized = false;
        let response = handle_message(
            &runtime,
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
        fs::write(default_dir.path().join("default.txt"), "alpha\n").expect("default write");
        fs::write(named_dir.path().join("named.txt"), "beta\n").expect("named write");

        let registry = registry(&args_with(
            vec![default_dir.path().to_path_buf()],
            vec![WorkspaceArg {
                name: "named".to_string(),
                path: named_dir.path().to_path_buf(),
            }],
        ))
        .expect("registry");
        let runtime = tools::runtime(registry);
        assert_eq!(runtime.resolver().len(), 2);

        let mut initialized = true;
        let response = handle_message(
            &runtime,
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
        fs::write(default_dir.path().join("default.txt"), "alpha\n").expect("default write");
        fs::write(dynamic_dir.path().join("dynamic.txt"), "dynamic\n").expect("dynamic write");

        let runtime = tools::runtime(
            registry(&args_with(
                vec![default_dir.path().to_path_buf()],
                Vec::new(),
            ))
            .expect("registry"),
        );
        let mut initialized = true;

        let add = handle_message(
            &runtime,
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
            &runtime,
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
            &runtime,
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
            &runtime,
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
