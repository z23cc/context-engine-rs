use crate::workspace::ServeArgs;
use crate::{agent, auth, commands, daemon, server};
use anyhow::Result;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "nerve",
    version,
    about = "Nerve Workstation: local AI runtime and MCP adapter"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Agent-facing MCP adapter commands.
    Mcp(McpArgs),
    /// Run the local Nerve Runtime daemon.
    Daemon(daemon::RuntimeDaemonArgs),
    /// Print local toolchain diagnostics.
    Doctor,
    /// Inspect configuration.
    Config(ConfigArgs),
    /// Warm the current project's semantic index cache.
    Warm(ServeArgs),
    /// Manage xAI OAuth credentials.
    Auth(auth::AuthArgs),
    /// Multi-provider agent loop: subscription login and task run.
    Agent(agent::AgentArgs),
    /// Manage local caches.
    Cache(CacheArgs),
    /// Register Nerve as an MCP server in Claude Code and/or Codex.
    Install(commands::install::InstallArgs),
    /// Interactive terminal chat client (forwards to the bundled `nerve-chat`).
    Chat(commands::chat::ChatArgs),
}

#[derive(Debug, Args)]
struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Run the JSON-RPC stdio MCP server.
    Serve(ServeArgs),
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

pub(crate) fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        CommandKind::Mcp(args) => match args.command {
            McpCommand::Serve(serve_args) => server::serve(serve_args),
        },
        CommandKind::Daemon(args) => daemon::run(args),
        CommandKind::Doctor => commands::doctor::doctor(),
        CommandKind::Config(args) => match args.command {
            ConfigCommand::Roots(serve_args) => commands::config::config_roots(serve_args),
        },
        CommandKind::Warm(args) => commands::cache::warm(args),
        CommandKind::Auth(args) => auth::run(args),
        CommandKind::Agent(args) => agent::run(args),
        CommandKind::Cache(args) => match args.command {
            CacheCommand::Purge(serve_args) => commands::cache::purge(serve_args),
        },
        CommandKind::Install(args) => commands::install::install(args),
        CommandKind::Chat(args) => commands::chat::chat(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_warm_cache_and_auth() {
        let daemon = Cli::try_parse_from(["nerve", "daemon", "--stdio", "--root", "."])
            .expect("daemon parse");
        assert!(matches!(daemon.command, CommandKind::Daemon(_)));
        let mcp =
            Cli::try_parse_from(["nerve", "mcp", "serve", "--root", "."]).expect("mcp serve parse");
        assert!(matches!(mcp.command, CommandKind::Mcp(_)));
        let warm = Cli::try_parse_from(["nerve", "warm"]).expect("warm parse");
        assert!(matches!(warm.command, CommandKind::Warm(_)));
        let purge = Cli::try_parse_from(["nerve", "cache", "purge"]).expect("purge parse");
        assert!(matches!(purge.command, CommandKind::Cache(_)));
        let login =
            Cli::try_parse_from(["nerve", "auth", "login", "xai"]).expect("auth login parse");
        assert!(matches!(login.command, CommandKind::Auth(_)));
        let status = Cli::try_parse_from(["nerve", "auth", "status"]).expect("status parse");
        assert!(matches!(status.command, CommandKind::Auth(_)));
        let logout = Cli::try_parse_from(["nerve", "auth", "logout"]).expect("logout parse");
        assert!(matches!(logout.command, CommandKind::Auth(_)));
    }

    #[test]
    fn cli_parses_daemon_http_transport() {
        let explicit =
            Cli::try_parse_from(["nerve", "daemon", "--http", "127.0.0.1:4173", "--root", "."])
                .expect("daemon http parse");
        assert!(matches!(explicit.command, CommandKind::Daemon(_)));
        // `--http` with no value falls back to the default loopback address.
        let defaulted = Cli::try_parse_from(["nerve", "daemon", "--http", "--root", "."])
            .expect("daemon http default parse");
        assert!(matches!(defaulted.command, CommandKind::Daemon(_)));
    }

    #[test]
    fn cli_parses_agent_run_allow_all_flag() {
        // The permission-bypass flag and its aliases are accepted.
        for flag in ["--allow-all", "--yes", "-y"] {
            let parsed = Cli::try_parse_from([
                "nerve",
                "agent",
                "run",
                "--provider",
                "claude",
                "--model",
                "m",
                flag,
                "do it",
            ])
            .unwrap_or_else(|err| panic!("agent run {flag} parse: {err}"));
            assert!(matches!(parsed.command, CommandKind::Agent(_)));
        }
        // ...and it is optional (gating defaults to on).
        let parsed = Cli::try_parse_from([
            "nerve",
            "agent",
            "run",
            "--provider",
            "claude",
            "--model",
            "m",
            "do it",
        ])
        .expect("agent run without allow-all");
        assert!(matches!(parsed.command, CommandKind::Agent(_)));
    }

    #[test]
    fn cli_parses_chat_flags() {
        // Flags are explicit and all optional — the saved default / picker fills
        // any gaps at runtime.
        let parsed = Cli::try_parse_from([
            "nerve",
            "chat",
            "--provider",
            "claude",
            "--model",
            "claude-sonnet-4",
            "--root",
            ".",
        ])
        .expect("chat parse");
        assert!(matches!(parsed.command, CommandKind::Chat(_)));
        let bare = Cli::try_parse_from(["nerve", "chat"]).expect("bare chat parse");
        assert!(matches!(bare.command, CommandKind::Chat(_)));
    }

    #[test]
    fn cli_parses_agent_sessions_subcommands() {
        let list =
            Cli::try_parse_from(["nerve", "agent", "sessions", "list"]).expect("sessions list");
        assert!(matches!(list.command, CommandKind::Agent(_)));
        let list_root = Cli::try_parse_from(["nerve", "agent", "sessions", "list", "--root", "."])
            .expect("sessions list --root");
        assert!(matches!(list_root.command, CommandKind::Agent(_)));
        let show =
            Cli::try_parse_from(["nerve", "agent", "sessions", "show", "20260618T120000Z-000"])
                .expect("sessions show");
        assert!(matches!(show.command, CommandKind::Agent(_)));
        let show_json = Cli::try_parse_from([
            "nerve",
            "agent",
            "sessions",
            "show",
            "--json",
            "20260618T120000Z-000",
        ])
        .expect("sessions show --json");
        assert!(matches!(show_json.command, CommandKind::Agent(_)));
    }
}
