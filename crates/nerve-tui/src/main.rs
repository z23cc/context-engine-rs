//! `nerve-tui` binary: a runtime-protocol client.
//!
//! Subcommands:
//!   nerve-tui smoke [--root PATH] [--binary PATH]   no-LLM round-trip
//!   nerve-tui       [--root PATH] [--binary PATH] [--provider P] [--model M]
//!                                                   interactive shell
//!
//! Args are parsed by hand (no clap) to keep the dep surface small, mirroring
//! the TS smoke parser; `--root` defaults to the current directory.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use nerve_tui::protocol::DaemonSpec;
use nerve_tui::{app, smoke};

#[derive(Debug, Default)]
struct Args {
    root: Option<PathBuf>,
    binary: Option<PathBuf>,
    provider: Option<String>,
    model: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    match run(raw).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nerve-tui: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(raw: Vec<String>) -> Result<()> {
    let (command, rest) = split_command(&raw);
    let args = parse_args(rest)?;
    let root = resolve_root(args.root)?;
    match command {
        Command::Smoke => {
            let spec = smoke::smoke_spec(root, args.binary);
            let report = smoke::run_smoke(spec).await?;
            println!("{}", report.pass_line());
            Ok(())
        }
        Command::Shell => {
            let provider = args.provider.unwrap_or_else(|| "claude".to_string());
            let model = args.model.unwrap_or_else(|| "claude-opus-4-8".to_string());
            let spec = DaemonSpec::new(root)
                .with_provider_model(Some(provider.clone()), Some(model.clone()));
            let spec = match args.binary {
                Some(binary) => spec.with_binary(binary),
                None => spec,
            };
            app::run(spec, provider, model).await
        }
    }
}

enum Command {
    Smoke,
    Shell,
}

/// Peel an optional leading subcommand off the args. `smoke` selects the smoke
/// round-trip; anything else (including nothing) is the interactive shell.
fn split_command(raw: &[String]) -> (Command, &[String]) {
    match raw.first().map(String::as_str) {
        Some("smoke") => (Command::Smoke, &raw[1..]),
        _ => (Command::Shell, raw),
    }
}

fn parse_args(raw: &[String]) -> Result<Args> {
    let mut args = Args::default();
    let mut iter = raw.iter();
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--root" => args.root = Some(PathBuf::from(value(&mut iter, "--root")?)),
            "--binary" => args.binary = Some(PathBuf::from(value(&mut iter, "--binary")?)),
            "--provider" => args.provider = Some(value(&mut iter, "--provider")?),
            "--model" => args.model = Some(value(&mut iter, "--model")?),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String> {
    iter.next()
        .cloned()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

/// Resolve `--root` to an absolute path, defaulting to the current directory.
fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
    let root = match root {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    if root.is_absolute() {
        Ok(root)
    } else {
        Ok(std::env::current_dir()?.join(root))
    }
}

fn print_usage() {
    println!(
        "usage:\n  \
         nerve-tui smoke [--root PATH] [--binary PATH]\n  \
         nerve-tui [--root PATH] [--binary PATH] [--provider P] [--model M]"
    );
}
