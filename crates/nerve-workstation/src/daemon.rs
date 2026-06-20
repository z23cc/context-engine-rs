mod http;
mod router;
mod setup;
mod stdio;

use crate::workspace;
use anyhow::{Result, bail};
use clap::Args;
use std::net::SocketAddr;

#[derive(Debug, Args)]
pub(crate) struct RuntimeDaemonArgs {
    /// Run the daemon over line-delimited JSON-RPC on stdin/stdout.
    #[arg(long)]
    stdio: bool,
    /// Serve the same Protocol v3 over HTTP for browser/GUI clients:
    /// `POST /rpc` (JSON-RPC) and `GET /events` (SSE). Binds the given loopback
    /// address (default `127.0.0.1:4173`). Mutually exclusive with `--stdio`.
    /// Both endpoints require a per-run bearer token (embedded in the GUI on a
    /// loopback bind); CORS is restricted to loopback origins.
    #[arg(long, value_name = "ADDR", num_args = 0..=1, default_missing_value = "127.0.0.1:4173")]
    http: Option<SocketAddr>,
    /// Permit binding the HTTP transport to a non-loopback address. Without it a
    /// non-loopback `--http` bind is refused. A remote bind still requires the
    /// bearer token (it is never anonymous) and is not embedded into the GUI.
    #[arg(long, requires = "http")]
    http_allow_remote: bool,
    /// Allow `delegate.start` jobs to spawn external coding-agent CLIs (codex /
    /// claude / gemini). Default off: the daemon refuses delegation, like it
    /// refuses `run_command` exec. The delegated agent runs with the workspace as
    /// its cwd, a scrubbed environment (its own on-disk login is used, never
    /// nerve's credentials), network allowed (it calls its LLM API), and a long
    /// wall-clock timeout. The delegated autonomy defaults to read-only; `edit` /
    /// `full` require the explicit `autonomy` argument on `delegate.start`.
    #[arg(long = "allow-delegate")]
    allow_delegate: bool,
    #[command(flatten)]
    serve: workspace::ServeArgs,
}

pub(crate) fn run(args: RuntimeDaemonArgs) -> Result<()> {
    // One transport per run: the stdio and HTTP transports drive the same
    // router but own their process I/O exclusively.
    match (args.stdio, args.http) {
        (true, Some(_)) => bail!("choose a single daemon transport: --stdio or --http <addr>"),
        (false, None) => bail!("daemon requires a transport: --stdio or --http <addr>"),
        (true, None) => stdio::run_stdio(args.serve, args.allow_delegate),
        (false, Some(addr)) => {
            if !addr.ip().is_loopback() && !args.http_allow_remote {
                bail!(
                    "refusing to bind the daemon HTTP transport to non-loopback {addr}: pass \
                     --http-allow-remote to expose it (the per-run bearer token is then required)"
                );
            }
            http::run_http(args.serve, addr, args.allow_delegate)
        }
    }
}

#[cfg(test)]
mod tests;
