//! Stdio JSON-RPC server and small CLI for the context engine.

mod agent;
mod agent_event;
mod agent_toolbox;
mod auth;
mod capabilities;
mod checkpoint;
mod cli;
mod commands;
mod daemon;
mod hooks;
mod jobs;
mod mcp;
mod memory;
mod openai;
mod policy;
mod providers;
mod rpc;
mod runconfig;
mod server;
mod session;
mod session_manager;
mod subagent;
mod tools;
mod workspace;
mod xai;

fn main() -> anyhow::Result<()> {
    cli::run()
}
