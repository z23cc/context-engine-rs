use super::WorkspaceArg;
use anyhow::{Context, Result, bail};
use clap::Args;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Args)]
pub(super) struct InstallArgs {
    /// Workspace root to expose. Repeatable. Defaults to the current directory.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
    /// Additional named workspace as name=path. Repeatable.
    #[arg(long = "workspace")]
    workspaces: Vec<WorkspaceArg>,
    /// Name to register the MCP server under.
    #[arg(long, default_value = "context-engine")]
    name: String,
    /// Configure Claude Code only (default: configure both).
    #[arg(long)]
    claude: bool,
    /// Configure Codex only (default: configure both).
    #[arg(long)]
    codex: bool,
    /// Claude Code config scope: local, user, or project.
    #[arg(long, default_value = "local")]
    scope: String,
    /// Override the ctx-mcp executable path written into the config.
    #[arg(long)]
    command: Option<PathBuf>,
    /// Print the commands that would run without executing them.
    #[arg(long)]
    dry_run: bool,
}

/// Decide which tools to configure: both unless exactly one flag is set.
pub(super) fn select_targets(claude: bool, codex: bool) -> (bool, bool) {
    if claude == codex {
        (true, true)
    } else {
        (claude, codex)
    }
}

pub(super) fn build_serve_args(roots: &[String], workspaces: &[(String, String)]) -> Vec<String> {
    let mut args = vec!["serve".to_string()];
    for root in roots {
        args.push("--root".to_string());
        args.push(root.clone());
    }
    for (name, path) in workspaces {
        args.push("--workspace".to_string());
        args.push(format!("{name}={path}"));
    }
    args
}

/// First match for `tool` on PATH, if any.
fn which(tool: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// The path to write into client configs. Prefers Homebrew's stable `bin`
/// symlink over the versioned Cellar path so configs survive `brew upgrade`.
fn resolve_command(override_cmd: Option<PathBuf>) -> Result<String> {
    if let Some(cmd) = override_cmd {
        return Ok(cmd.to_string_lossy().into_owned());
    }
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let exe_str = exe.to_string_lossy();
    if let Some(idx) = exe_str.find("/Cellar/ctx-mcp/") {
        let linked = PathBuf::from(format!("{}/bin/ctx-mcp", &exe_str[..idx]));
        if linked.exists() {
            return Ok(linked.to_string_lossy().into_owned());
        }
    }
    Ok(exe_str.into_owned())
}

fn canonical(path: &Path) -> Result<String> {
    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("path does not exist: {}", path.display()))?;
    Ok(abs.to_string_lossy().into_owned())
}

pub(super) fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | '=' | ':' | ',' | '+')
        });
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn print_command(program: &str, args: &[String]) {
    let mut line = String::from(program);
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    println!("{line}");
}

fn run_tool(program: &str, args: &[String]) -> Result<bool> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    Ok(status.success())
}

pub(super) fn install(args: InstallArgs) -> Result<()> {
    let command = resolve_command(args.command.clone())?;

    let mut roots: Vec<String> = Vec::new();
    if args.roots.is_empty() {
        roots.push(canonical(
            &std::env::current_dir().context("failed to read current directory")?,
        )?);
    } else {
        for root in &args.roots {
            roots.push(canonical(root)?);
        }
    }
    let workspaces: Vec<(String, String)> = args
        .workspaces
        .iter()
        .map(|w| Ok((w.name.clone(), canonical(&w.path)?)))
        .collect::<Result<_>>()?;

    let serve_args = build_serve_args(&roots, &workspaces);
    let (do_claude, do_codex) = select_targets(args.claude, args.codex);

    println!("ctx-mcp: {command}");
    println!("roots:   {}", roots.join(", "));
    if !workspaces.is_empty() {
        let rendered: Vec<String> = workspaces.iter().map(|(n, p)| format!("{n}={p}")).collect();
        println!("workspaces: {}", rendered.join(", "));
    }
    println!();

    let mut configured = false;
    if do_claude {
        configured |=
            configure_claude(&args.name, &args.scope, &command, &serve_args, args.dry_run)?;
    }
    if do_codex {
        configured |= configure_codex(&args.name, &command, &serve_args, args.dry_run)?;
    }

    if configured && !args.dry_run {
        println!();
        println!(
            "Done. Restart Claude Code / Codex to pick up '{}'.",
            args.name
        );
    }
    Ok(())
}

fn configure_claude(
    name: &str,
    scope: &str,
    command: &str,
    serve_args: &[String],
    dry_run: bool,
) -> Result<bool> {
    let mut add = vec![
        "mcp".to_string(),
        "add".to_string(),
        "-s".to_string(),
        scope.to_string(),
        name.to_string(),
        "--".to_string(),
        command.to_string(),
    ];
    add.extend(serve_args.iter().cloned());

    if which("claude").is_none() {
        println!("\u{2022} Claude Code: `claude` CLI not found \u{2014} add it manually:");
        print!("    ");
        print_command("claude", &add);
        return Ok(false);
    }
    if dry_run {
        print_command("claude", &add);
        return Ok(true);
    }
    // idempotent: drop any existing entry in this scope first
    let _ = Command::new("claude")
        .args(["mcp", "remove", "-s", scope, name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if run_tool("claude", &add)? {
        println!("\u{2713} Claude Code: registered '{name}' (scope: {scope})");
        Ok(true)
    } else {
        bail!("claude mcp add failed");
    }
}

fn configure_codex(
    name: &str,
    command: &str,
    serve_args: &[String],
    dry_run: bool,
) -> Result<bool> {
    let mut add = vec![
        "mcp".to_string(),
        "add".to_string(),
        name.to_string(),
        "--".to_string(),
        command.to_string(),
    ];
    add.extend(serve_args.iter().cloned());

    if which("codex").is_none() {
        println!("\u{2022} Codex: `codex` CLI not found \u{2014} add it manually:");
        print!("    ");
        print_command("codex", &add);
        return Ok(false);
    }
    if dry_run {
        print_command("codex", &add);
        return Ok(true);
    }
    let _ = Command::new("codex")
        .args(["mcp", "remove", name])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if run_tool("codex", &add)? {
        println!("\u{2713} Codex: registered '{name}'");
        Ok(true)
    } else {
        bail!("codex mcp add failed");
    }
}
