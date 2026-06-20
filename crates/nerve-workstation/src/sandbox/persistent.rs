//! `PersistentChild` — the live, multi-turn variant of a contained spawn.
//!
//! [`super::SandboxLauncher::launch_streaming`] runs one command to completion and
//! reaps it; this is the *steerable* shape (DA-5a): the child stays alive across
//! turns and reads further input from an **open** stdin, exiting only when that
//! stdin reaches EOF (the close/cancel path). The handle therefore exposes three
//! things the one-shot path does not: a stdin writer kept open between turns, a
//! stdout line stream a reader thread feeds continuously, and explicit
//! kill/reap controls (no wall-clock timeout — a live session is bounded by the
//! caller's close, not a single command's clock).
//!
//! The same containment as [`super::ProcessLauncher`] applies — forced cwd,
//! env-clear + scrubbed allowlist, isolated process group — because the child is
//! spawned through the shared [`super::process::spawn_child`] helper.

use super::process::{kill_process_tree, spawn_child};
use super::{CommandSpec, SandboxPolicy};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::process::Child;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

/// A live contained child: an open stdin (write more input across turns), a
/// stdout line stream (a reader thread sends each newline-terminated line), and
/// kill/wait controls. Dropping the handle does **not** kill the child; call
/// [`Self::close`] (EOF on stdin) or [`Self::kill`] explicitly so the lifecycle is
/// always deliberate.
pub(crate) struct PersistentChild {
    child: Child,
    /// The child's stdin, kept open between turns. Taken (and dropped → EOF) by
    /// [`Self::close`]. Behind a `Mutex` so a writer and a closer can't race.
    stdin: Mutex<Option<std::process::ChildStdin>>,
    /// Receives every newline-terminated stdout line as the reader thread reads
    /// it. The sender lives on the reader thread; when the child closes stdout the
    /// thread ends and the channel disconnects.
    lines: Receiver<String>,
}

impl PersistentChild {
    /// Spawn a long-lived contained child for `spec` under `policy` with a piped
    /// stdin/stdout, starting a reader thread that forwards each stdout line over
    /// the returned handle's line channel.
    pub(crate) fn spawn(spec: &CommandSpec, policy: &SandboxPolicy) -> Result<Self> {
        use std::process::Stdio;
        let mut child = spawn_child(spec, policy, Stdio::piped())?;
        let stdin = child
            .stdin
            .take()
            .context("persistent child stdin missing")?;
        let stdout = child
            .stdout
            .take()
            .context("persistent child stdout missing")?;
        let (line_tx, lines) = channel::<String>();
        std::thread::spawn(move || read_lines(stdout, &line_tx));
        Ok(Self {
            child,
            stdin: Mutex::new(Some(stdin)),
            lines,
        })
    }

    /// Write one message to the child's stdin as a single framed write (the
    /// caller supplies the full line, including its trailing `\n`), then flush.
    /// Errors if stdin was already closed.
    pub(crate) fn write_line(&self, line: &str) -> Result<()> {
        let mut guard = crate::sync::lock_recover(&self.stdin);
        let stdin = guard
            .as_mut()
            .context("persistent child stdin already closed")?;
        stdin
            .write_all(line.as_bytes())
            .context("writing to persistent child stdin")?;
        stdin.flush().context("flushing persistent child stdin")?;
        Ok(())
    }

    /// Borrow the stdout line receiver so the caller can drain turn output. Each
    /// item is one stdout line with its line terminator stripped.
    pub(crate) fn lines(&self) -> &Receiver<String> {
        &self.lines
    }

    /// Close the child's stdin (drop it → EOF), the graceful end-of-session signal
    /// a stdin-driven agent exits on. Idempotent: a second call is a no-op.
    pub(crate) fn close_stdin(&self) {
        let mut guard = crate::sync::lock_recover(&self.stdin);
        // Dropping the `ChildStdin` closes the write end → the child reads EOF.
        let _ = guard.take();
    }

    /// SIGKILL the child's whole process group (forked grandchildren included) for
    /// an immediate, non-graceful teardown (cancel / interrupt that did not settle).
    pub(crate) fn kill(&mut self) {
        kill_process_tree(&mut self.child);
    }

    /// Reap the child, returning its exit code (`None` when killed by signal).
    /// Closes stdin first so a child still blocked on a read observes EOF and
    /// exits, rather than the reap blocking forever.
    pub(crate) fn wait(&mut self) -> Result<Option<i32>> {
        self.close_stdin();
        let status = self.child.wait().context("reaping persistent child")?;
        Ok(status.code())
    }

    /// Whether the child has already exited (non-blocking).
    pub(crate) fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// Read `reader` line by line, forwarding each newline-terminated line (terminator
/// stripped) over `lines` until EOF. Best-effort: a disconnected receiver (the
/// session was dropped) just ends the loop. No cap — a live session's output is
/// consumed turn-by-turn by the caller, not retained as one bounded blob.
fn read_lines(reader: impl std::io::Read, lines: &Sender<String>) {
    let mut buf = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if lines
                    .send(line.trim_end_matches(['\n', '\r']).to_string())
                    .is_err()
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
