//! `nerve agent sessions` — read-only browsing of persisted run transcripts.
//!
//! A self-contained CLI face over [`SessionStore`] (P5 persistence): list recent
//! sessions or print one transcript. It never runs the agent loop, so it lives
//! apart from the run/login orchestration in [`super`].

use crate::session::SessionStore;
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub(super) struct SessionsArgs {
    #[command(subcommand)]
    command: SessionsCommand,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    /// List recent agent sessions, most recent first.
    List(SessionsScopeArgs),
    /// Print a stored session transcript.
    Show(SessionsShowArgs),
}

#[derive(Debug, Args)]
struct SessionsScopeArgs {
    /// Project root whose `.nerve/sessions` is read. Defaults to the current
    /// directory; pass the same `--root` you ran the agent with.
    #[arg(long = "root")]
    root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SessionsShowArgs {
    #[command(flatten)]
    scope: SessionsScopeArgs,
    /// Session id, as shown by `nerve agent sessions list`.
    id: String,
    /// Print the raw stored JSON instead of a formatted transcript.
    #[arg(long)]
    json: bool,
}

pub(super) fn sessions(args: SessionsArgs) -> Result<()> {
    match args.command {
        SessionsCommand::List(scope) => sessions_list(&scope),
        SessionsCommand::Show(show) => sessions_show(&show),
    }
}

/// Resolve the session store for a browse scope. `--root` defaults to the current
/// directory so `sessions list` works from inside a project; with neither a root
/// nor a usable current directory, the global config home is used.
fn sessions_store(scope: &SessionsScopeArgs) -> Result<SessionStore> {
    let root = scope.root.clone().or_else(|| std::env::current_dir().ok());
    SessionStore::for_scope(root.as_deref())
}

fn sessions_list(scope: &SessionsScopeArgs) -> Result<()> {
    let store = sessions_store(scope)?;
    let records = store.list()?;
    if records.is_empty() {
        println!("no sessions in {}", store.dir().display());
        return Ok(());
    }
    for record in &records {
        println!("{}", record.summary_line());
    }
    Ok(())
}

fn sessions_show(args: &SessionsShowArgs) -> Result<()> {
    let store = sessions_store(&args.scope)?;
    if args.json {
        println!("{}", store.read_raw(&args.id)?);
    } else {
        print!("{}", store.load(&args.id)?.render_transcript());
    }
    Ok(())
}
