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
    /// Manage xAI OAuth credentials.
    Auth(auth::AuthArgs),
    /// Multi-provider agent loop: subscription login and task run.
    Agent(agent::AgentArgs),
    /// Register Nerve as an MCP server in Claude Code and/or Codex.
    Install(commands::install::InstallArgs),
    /// Interactive terminal chat client (forwards to the bundled `nerve-tui`).
    Chat(commands::chat::ChatArgs),
    /// Re-verify a captured run against its sealed Verification Receipt (L2/L4).
    Verify(commands::gate::VerifyArgs),
    /// Translate a sealed Receipt into a merge-gate decision + exit code (L5).
    Gate(commands::gate::GateArgs),
    /// Inspect the L1 cross-run evidence ledger (re-derive its hash chain).
    Ledger(LedgerArgs),
    /// Record a REAL post-merge outcome into a run's L6 corpus (advisory; never a gate).
    Outcome(commands::outcome::OutcomeArgs),
    /// EXPERIMENTAL: run a declarative agent-orchestration workflow (C1). Hidden
    /// from help; off-protocol, the WorkflowDef shape is not yet a stable contract.
    #[command(hide = true)]
    Flow(FlowArgs),
}

#[derive(Debug, Args)]
struct LedgerArgs {
    #[command(subcommand)]
    command: LedgerCommand,
}

#[derive(Debug, Subcommand)]
enum LedgerCommand {
    /// Re-derive the evidence ledger's hash chain and report whether it is intact
    /// (read-only; exit 0 intact, non-zero on a detected tamper).
    Verify(commands::ledger::LedgerVerifyArgs),
    /// Read the append-only evidence ledger offline, optionally filtered by run /
    /// run-root-hash lineage / record kind / agent (read-only; exit 0).
    Query(commands::ledger::LedgerQueryArgs),
}

#[derive(Debug, Args)]
struct FlowArgs {
    #[command(subcommand)]
    command: FlowCommand,
}

#[derive(Debug, Subcommand)]
enum FlowCommand {
    /// Run a `WorkflowDef` (read from `--file`) through the orchestration engine.
    Run(commands::flow::FlowArgs),
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
        CommandKind::Auth(args) => auth::run(args),
        CommandKind::Agent(args) => agent::run(args),
        CommandKind::Install(args) => commands::install::install(args),
        CommandKind::Chat(args) => commands::chat::chat(args),
        // verify/gate are CI surfaces whose exit code IS the gate output, so propagate
        // it to the process (never returns on the Ok path; `?` surfaces a real error).
        CommandKind::Verify(args) => {
            let code = commands::gate::verify(args)?;
            std::process::exit(code)
        }
        CommandKind::Gate(args) => {
            let code = commands::gate::gate(args)?;
            std::process::exit(code)
        }
        // `ledger verify` is a CI surface whose exit code IS the verdict (0 intact,
        // non-zero on a detected tamper), so propagate it to the process.
        CommandKind::Ledger(args) => match args.command {
            LedgerCommand::Verify(verify_args) => {
                let code = commands::ledger::verify(verify_args)?;
                std::process::exit(code)
            }
            // `ledger query` is a read-only audit surface; it always exits 0 (a query
            // that matches nothing is still a successful read).
            LedgerCommand::Query(query_args) => {
                let code = commands::ledger::query(query_args)?;
                std::process::exit(code)
            }
        },
        // `outcome` is the L6 ingestion rail (the offline twin of the daemon's
        // `outcome.label`): it records a REAL post-merge disposition + mirrors it onto
        // the L1 ledger, then exits 0 (a recording failure surfaces as a real error).
        CommandKind::Outcome(args) => {
            let code = commands::outcome::record(args)?;
            std::process::exit(code)
        }
        CommandKind::Flow(args) => match args.command {
            FlowCommand::Run(flow_args) => commands::flow::run(flow_args),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_verify_and_gate() {
        let verify =
            Cli::try_parse_from(["nerve", "verify", "run-abc", "--root", ".", "--reruns", "3"])
                .expect("verify parse");
        assert!(matches!(verify.command, CommandKind::Verify(_)));
        let gate = Cli::try_parse_from(["nerve", "gate", "--receipt", "r.json", "--emit", "gh"])
            .expect("gate parse");
        assert!(matches!(gate.command, CommandKind::Gate(_)));
        // The CI template's `nerve gate --run <id>` convenience parses too.
        let gate_run = Cli::try_parse_from(["nerve", "gate", "--run", "run-abc", "--root", "."])
            .expect("gate --run parse");
        assert!(matches!(gate_run.command, CommandKind::Gate(_)));
        // --receipt and --run are mutually exclusive.
        assert!(
            Cli::try_parse_from(["nerve", "gate", "--receipt", "r.json", "--run", "x"]).is_err(),
            "--receipt conflicts with --run"
        );
    }

    #[test]
    fn cli_parses_ledger_verify() {
        let bare = Cli::try_parse_from(["nerve", "ledger", "verify"]).expect("ledger verify parse");
        assert!(matches!(bare.command, CommandKind::Ledger(_)));
        let with_flags =
            Cli::try_parse_from(["nerve", "ledger", "verify", "--root", ".", "--json"])
                .expect("ledger verify flags parse");
        assert!(matches!(with_flags.command, CommandKind::Ledger(_)));
    }

    #[test]
    fn cli_parses_ledger_query() {
        let bare = Cli::try_parse_from(["nerve", "ledger", "query"]).expect("ledger query parse");
        assert!(matches!(bare.command, CommandKind::Ledger(_)));
        let with_flags = Cli::try_parse_from([
            "nerve",
            "ledger",
            "query",
            "--root",
            ".",
            "--run-root-hash",
            "root-abc",
            "--record-kind",
            "verdict",
            "--agent",
            "claude",
            "--limit",
            "10",
            "--json",
        ])
        .expect("ledger query flags parse");
        assert!(matches!(with_flags.command, CommandKind::Ledger(_)));
    }

    #[test]
    fn cli_parses_outcome() {
        // The bare post-merge form a CI hook uses.
        let merged = Cli::try_parse_from([
            "nerve", "outcome", "merged", "--run", "run-abc", "--root", ".",
        ])
        .expect("outcome merged parse");
        assert!(matches!(merged.command, CommandKind::Outcome(_)));
        // The full flag surface, incl. the multi-word disposition + source/receipt/session.
        let full = Cli::try_parse_from([
            "nerve",
            "outcome",
            "shipped-no-regress",
            "--run",
            "run-abc",
            "--receipt",
            "rcpt-1",
            "--session",
            "job-2",
            "--source",
            "ci",
            "--note",
            "post-merge hook",
            "--root",
            ".",
            "--json",
        ])
        .expect("outcome full flags parse");
        assert!(matches!(full.command, CommandKind::Outcome(_)));
        // A bogus disposition is rejected (the value-enum is the allowlist).
        assert!(
            Cli::try_parse_from(["nerve", "outcome", "bogus", "--run", "x"]).is_err(),
            "an unknown disposition must not parse"
        );
    }

    #[test]
    fn cli_parses_daemon_mcp_and_auth() {
        let daemon = Cli::try_parse_from(["nerve", "daemon", "--stdio", "--root", "."])
            .expect("daemon parse");
        assert!(matches!(daemon.command, CommandKind::Daemon(_)));
        let mcp =
            Cli::try_parse_from(["nerve", "mcp", "serve", "--root", "."]).expect("mcp serve parse");
        assert!(matches!(mcp.command, CommandKind::Mcp(_)));
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
    fn cli_parses_hidden_flow_run() {
        // The experimental engine driver is hidden from help but still parses.
        let parsed =
            Cli::try_parse_from(["nerve", "flow", "run", "--file", "plan.json", "--root", "."])
                .expect("flow run parse");
        assert!(matches!(parsed.command, CommandKind::Flow(_)));
        // The allow-all alias and concurrency override are accepted.
        let with_flags = Cli::try_parse_from([
            "nerve",
            "flow",
            "run",
            "--file",
            "plan.json",
            "--concurrency",
            "2",
            "-y",
        ])
        .expect("flow run flags parse");
        assert!(matches!(with_flags.command, CommandKind::Flow(_)));
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
