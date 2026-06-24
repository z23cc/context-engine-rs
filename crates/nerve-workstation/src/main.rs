//! Stdio JSON-RPC server and small CLI for the context engine.

mod agent;
mod agent_event;
mod agent_path;
mod agent_toolbox;
mod auth;
mod capabilities;
mod checkpoint;
mod cli;
mod commands;
mod cost;
mod daemon;
mod delegate;
mod delegate_codex_mcp;
mod delegate_live;
mod delegate_proxy;
mod delegate_roles;
mod delegate_runtime;
mod delegate_session;
mod delegate_session_codex;
mod delegate_store;
mod delegate_tool;
mod discovery;
mod exec_tool;
mod flow;
mod flow_job;
mod flow_remote;
mod flow_store;
mod hooks;
mod jobs;
mod ledger_store;
mod mcp;
mod memory;
mod openai;
mod otel_ingest;
mod outcome_store;
mod policy;
mod policy_plane;
mod providers;
mod receipt_store;
mod replay;
mod rpc;
mod run_store;
mod runconfig;
mod sandbox;
mod server;
mod session;
mod session_manager;
mod signer;
mod subagent;
mod substrate_mcp;
mod sync;
mod toolchain_pin;
mod tools;
mod verify_runner;
mod verify_store;
mod wechat;
mod worker;
mod workspace;
mod xai;

fn main() -> anyhow::Result<()> {
    cli::run()
}
