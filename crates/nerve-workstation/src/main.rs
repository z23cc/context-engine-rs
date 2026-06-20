//! Stdio JSON-RPC server and small CLI for the context engine.

mod agent;
mod agent_event;
mod agent_toolbox;
mod auth;
mod capabilities;
mod checkpoint;
mod cli;
mod commands;
mod cost;
mod daemon;
mod delegate;
mod delegate_runtime;
mod delegate_tool;
mod exec_tool;
mod hooks;
mod jobs;
mod mcp;
mod memory;
mod openai;
mod policy;
mod providers;
mod rpc;
mod runconfig;
mod sandbox;
mod server;
mod session;
mod session_manager;
mod subagent;
mod sync;
mod tools;
mod workspace;
mod xai;

fn main() -> anyhow::Result<()> {
    cli::run()
}
