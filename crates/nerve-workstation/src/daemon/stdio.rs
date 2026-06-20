use crate::rpc::{RpcMessage, jsonrpc_error, write_response};
use crate::workspace;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

pub(super) fn run_stdio(serve_args: workspace::ServeArgs, allow_delegate: bool) -> Result<()> {
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let notification_stdout = Arc::clone(&stdout);
    let router = super::setup::build_router(&serve_args, allow_delegate, move |value| {
        let _ = write_locked(&notification_stdout, value);
    })?;
    let stdin = io::stdin();

    for line in stdin.lock().lines() {
        let line = line.context("failed to read stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: RpcMessage = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                write_locked(&stdout, jsonrpc_error(Value::Null, -32700, err.to_string()))?;
                continue;
            }
        };

        router.handle_message(request, |value| write_locked(&stdout, value))?;
    }
    Ok(())
}

fn write_locked(out: &Arc<Mutex<impl Write>>, value: Value) -> Result<()> {
    let mut out = out.lock().map_err(|_| anyhow!("stdout lock poisoned"))?;
    write_response(&mut *out, value)
}
