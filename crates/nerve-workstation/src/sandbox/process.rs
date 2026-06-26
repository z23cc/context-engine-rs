//! `ProcessLauncher` — the MVP "trusted local dev" containment backend.
//!
//! Best-effort isolation via `std::process::Command`: a forced working
//! directory, an environment scrubbed down to an allowlist (with secrets removed
//! unconditionally), a hard wall-clock timeout that kills the whole process
//! group, and a cap on captured output. This is **not** strong isolation — a
//! determined process can still escape on a dev box (e.g. by daemonizing out of
//! its process group, or by reading absolute paths) — but it honestly bounds the
//! blast radius for a developer who already runs these commands by hand. Strong,
//! kernel-enforced backends (Landlock/seccomp on Linux) land later behind the
//! same [`SandboxLauncher`](super::SandboxLauncher) port without changing callers.

use super::{CommandSpec, EnvPolicy, Output, SandboxLauncher, SandboxPolicy};
use anyhow::{Context, Result};
use nerve_core::CancelToken;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// How often to poll a running child while waiting for it to exit, time out, or
/// be cancelled.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Best-effort confined-subprocess launcher (see module docs).
pub(crate) struct ProcessLauncher;

impl SandboxLauncher for ProcessLauncher {
    fn launch(
        &self,
        spec: &CommandSpec,
        policy: &SandboxPolicy,
        cancel: &CancelToken,
    ) -> Result<Output> {
        let mut child = spawn_child(spec, policy, Stdio::null())?;

        // Drain both pipes on dedicated threads so a chatty child can never
        // deadlock on a full pipe buffer while we poll for the timeout. On a
        // timeout/cancel kill we SIGKILL the whole group, which closes every
        // inherited write end, so these reads reach EOF and the joins return.
        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;
        let cap = policy.max_output;
        let stdout_reader = std::thread::spawn(move || read_capped(stdout, cap));
        let stderr_reader = std::thread::spawn(move || read_capped(stderr, cap));

        let (status, timed_out) = wait_with_timeout(&mut child, policy.timeout, cancel)?;

        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        Ok(Output {
            exit_code: status.code(),
            stdout,
            stderr,
            timed_out,
        })
    }

    fn launch_streaming(
        &self,
        spec: &CommandSpec,
        policy: &SandboxPolicy,
        stdin: &str,
        cancel: &CancelToken,
        on_line: &mut dyn FnMut(&str),
    ) -> Result<Output> {
        let mut child = spawn_child(spec, policy, Stdio::piped())?;
        // Write the task to the child's stdin on its own thread and drop the
        // pipe (EOF), so a large task never deadlocks against a child that is
        // still emitting stdout before it finishes reading stdin.
        write_stdin(&mut child, stdin.to_owned());

        let stdout = child.stdout.take().context("child stdout missing")?;
        let stderr = child.stderr.take().context("child stderr missing")?;
        let cap = policy.max_output;
        // stdout is streamed line-by-line over a channel so the caller sees
        // progress live; the captured text is still cap-bounded for the result.
        let (line_tx, line_rx) = channel::<String>();
        let stdout_reader = std::thread::spawn(move || stream_capped(stdout, cap, &line_tx));
        let stderr_reader = std::thread::spawn(move || read_capped(stderr, cap));

        let timed_out = drive_streaming(&mut child, policy.timeout, cancel, &line_rx, on_line)?;
        // Drain any lines produced between the last poll and EOF.
        drain_lines(&line_rx, on_line);

        let status = child.wait().context("reaping streamed child")?;
        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        Ok(Output {
            exit_code: status.code(),
            stdout,
            stderr,
            timed_out,
        })
    }

    fn launch_persistent(
        &self,
        spec: &CommandSpec,
        policy: &SandboxPolicy,
    ) -> Result<super::PersistentChild> {
        super::PersistentChild::spawn(spec, policy)
    }

    /// Best-effort process containment — scrubbed+pinned env and a forced cwd, but NO
    /// kernel closure (net-deny is intent, `fs_read`/`fs_write` unenforced). The honest
    /// probed tier is [`IsolationTier::Contained`] (INV-R7); it must never claim
    /// `Hermetic` until a kernel backend lands.
    fn isolation_tier(&self) -> nerve_core::provenance::IsolationTier {
        nerve_core::provenance::IsolationTier::Contained
    }
}

/// Build and spawn the contained child with the shared containment (forced cwd,
/// env-clear + scrubbed allowlist, isolated process group). `stdin` selects the
/// stdin disposition (`null` for [`ProcessLauncher::launch`], a pipe for the
/// streaming variant that feeds the task in).
pub(super) fn spawn_child(
    spec: &CommandSpec,
    policy: &SandboxPolicy,
    stdin: Stdio,
) -> Result<Child> {
    // Resolve a bare program name (e.g. `claude`) against the repaired PATH so a
    // GUI-launched daemon — which inherits a minimal launchd PATH — can still find
    // user-local agent CLIs; a name with a path separator is taken verbatim.
    let resolved = crate::agent_path::resolve_program(&spec.command);
    let program = resolved
        .clone()
        .unwrap_or_else(|| PathBuf::from(&spec.command));
    let mut command = Command::new(&program);
    command
        .args(&spec.args)
        .current_dir(&policy.cwd)
        .env_clear()
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    isolate_process_group(&mut command);
    for (name, value) in child_env(std::env::vars(), &policy.env) {
        command.env(name, value);
    }
    // Explicit overrides win over the inherited+scrubbed environment (e.g. the
    // repaired PATH the delegate runtime injects for agent spawns).
    for (name, value) in &policy.env_overrides {
        command.env(name, value);
    }
    command
        .spawn()
        .with_context(|| spawn_error_context(spec, resolved.is_none()))
}

/// Context for a failed spawn. A bare program name that did not resolve on the
/// (repaired) PATH gets an actionable "not found" hint — the common delegate
/// failure when an agent CLI (claude/codex/gemini) isn't installed or is off the
/// daemon's PATH — instead of the bare io error.
fn spawn_error_context(spec: &CommandSpec, unresolved: bool) -> String {
    if unresolved && !spec.command.contains('/') {
        format!(
            "failed to spawn `{}`: not found on PATH — is it installed and on the daemon's PATH?",
            spec.command
        )
    } else {
        format!("failed to spawn `{}`", spec.command)
    }
}

/// Feed `input` to the child's stdin on a detached thread, then close the pipe so
/// the child observes EOF. Errors (e.g. the child closed stdin early) are ignored:
/// a broken-pipe write must not abort the run.
fn write_stdin(child: &mut Child, input: String) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
        // Dropping `stdin` here closes the write end → the child reads EOF.
    });
}

/// Poll the child to exit / time out / cancel, forwarding any streamed stdout
/// lines to `on_line` between polls so progress is live. Returns whether the
/// child was killed by the wall-clock timeout. The actual reap is the caller's.
fn drive_streaming(
    child: &mut Child,
    timeout: Duration,
    cancel: &CancelToken,
    lines: &Receiver<String>,
    on_line: &mut dyn FnMut(&str),
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        drain_lines(lines, on_line);
        if child
            .try_wait()
            .context("polling streamed child")?
            .is_some()
        {
            return Ok(false);
        }
        if cancel.is_cancelled() {
            kill_process_tree(child);
            return Ok(false);
        }
        if Instant::now() >= deadline {
            if child.try_wait().context("final streamed status")?.is_some() {
                return Ok(false);
            }
            kill_process_tree(child);
            return Ok(true);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Forward every line currently buffered in the channel to `on_line` without
/// blocking (the producer thread sends as it reads; this is the consumer side).
fn drain_lines(lines: &Receiver<String>, on_line: &mut dyn FnMut(&str)) {
    while let Ok(line) = lines.try_recv() {
        on_line(&line);
    }
}

/// Wait for `child`, killing its process group when `timeout` elapses or `cancel`
/// fires. Returns the (possibly signal-killed) exit status and whether the kill
/// was due to the timeout (cancellation is reported by the caller via the token).
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<(ExitStatus, bool)> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().context("polling child status")? {
            return Ok((status, false));
        }
        if cancel.is_cancelled() {
            kill_process_tree(child);
            let status = child.wait().context("reaping cancelled child")?;
            return Ok((status, false));
        }
        if Instant::now() >= deadline {
            // The child may have exited in the same tick the deadline passed;
            // prefer the real status over a spurious timeout.
            if let Some(status) = child.try_wait().context("final child status")? {
                return Ok((status, false));
            }
            kill_process_tree(child);
            let status = child.wait().context("reaping timed-out child")?;
            return Ok((status, true));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Put the child in its own process group so a group-kill reaches the whole tree
/// and the terminal's Ctrl-C does not hit it directly (nerve flips the cancel
/// token and tears the group down deterministically instead).
#[cfg(unix)]
fn isolate_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn isolate_process_group(_command: &mut Command) {}

/// SIGKILL the child's whole process group, so forked grandchildren (e.g.
/// `rustc`, build scripts, `node`) die too and release the stdout/stderr pipes.
/// Killing only the direct child would leave them holding the pipes open, and
/// the reader joins would block past the timeout — defeating the wall-clock bound.
/// Called only while the group leader is still alive (before reaping), so the
/// group id cannot have been recycled.
#[cfg(unix)]
pub(super) fn kill_process_tree(child: &mut Child) {
    let pgid = child.id() as libc::pid_t;
    // SAFETY: `killpg` is async-signal-safe; we target the child's own process
    // group, created via `process_group(0)`, and the leader has not been reaped.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
pub(super) fn kill_process_tree(child: &mut Child) {
    let _ = child.kill();
}

/// Build the child environment from the parent's, keeping only allowlisted names
/// and dropping any secret-shaped name unconditionally (defence in depth: even an
/// allowlisted prefix cannot leak a token/key/secret).
pub(crate) fn child_env(
    parent: impl Iterator<Item = (String, String)>,
    policy: &EnvPolicy,
) -> Vec<(String, String)> {
    parent
        .filter(|(name, _)| policy.allows(name) && !is_secret_name(name))
        .collect()
}

/// Whether an environment variable name looks like a credential. Matched
/// case-insensitively so the scrub catches `GITHUB_TOKEN`, `OPENAI_API_KEY`,
/// `AWS_SECRET_ACCESS_KEY`, `NPM_CONFIG__AUTH`, `*_PASSWORD`, etc. Name-based
/// only: credentials embedded in a variable's *value* (e.g. a URL with
/// `user:pass`) are not detectable here and are not sanitized.
pub(crate) fn is_secret_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_TOKEN")
        || upper.ends_with("_KEY")
        || upper.ends_with("_SECRET")
        || upper.contains("SECRET")
        || upper.contains("TOKEN")
        || upper.contains("APIKEY")
        || upper.contains("PASSWORD")
        || upper.contains("PASSWD")
        || upper.contains("PASSPHRASE")
        || upper.contains("AUTH")
        || upper.contains("CRED")
}

/// Read `reader` to EOF, retaining at most `cap` bytes split between the **head**
/// and **tail** of the stream (and draining the rest so the writer never blocks).
/// Head+tail matters for build tools whose failure summary lands at the end.
/// Returns the captured text (UTF-8 lossy), with a marker inserted when more than
/// `cap` bytes were produced.
fn read_capped(mut reader: impl Read, cap: usize) -> String {
    let head_cap = cap / 2;
    let tail_cap = cap - head_cap;
    let mut head: Vec<u8> = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut total = 0usize;
    let mut scratch = [0u8; 8192];
    loop {
        match reader.read(&mut scratch) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                append_capped(&mut head, &mut tail, head_cap, tail_cap, &scratch[..n]);
            }
            Err(_) => break,
        }
    }
    finalize_output(&head, &tail, total, cap)
}

/// Read `reader` line by line, sending each newline-terminated line over `lines`
/// for live forwarding while *also* accumulating the cap-bounded capture that
/// becomes the run's `stdout`. The send is best-effort: if the consumer hung up
/// (e.g. the run was cancelled), the line is still captured. Returns the same
/// head+tail capped string as [`read_capped`].
fn stream_capped(reader: impl Read, cap: usize, lines: &Sender<String>) -> String {
    let head_cap = cap / 2;
    let tail_cap = cap - head_cap;
    let mut head: Vec<u8> = Vec::new();
    let mut tail: VecDeque<u8> = VecDeque::new();
    let mut total = 0usize;
    let mut buf = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match buf.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                total += line.len();
                append_capped(&mut head, &mut tail, head_cap, tail_cap, line.as_bytes());
                let _ = lines.send(line.trim_end_matches(['\n', '\r']).to_string());
            }
            Err(_) => break,
        }
    }
    finalize_output(&head, &tail, total, cap)
}

/// Append `chunk` to the head buffer until it is full, then to a bounded tail
/// ring buffer — so the capture keeps both the start and the end of the stream.
fn append_capped(
    head: &mut Vec<u8>,
    tail: &mut VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    chunk: &[u8],
) {
    let mut chunk = chunk;
    if head.len() < head_cap {
        let take = (head_cap - head.len()).min(chunk.len());
        head.extend_from_slice(&chunk[..take]);
        chunk = &chunk[take..];
    }
    if tail_cap == 0 || chunk.is_empty() {
        return;
    }
    if chunk.len() >= tail_cap {
        tail.clear();
        tail.extend(&chunk[chunk.len() - tail_cap..]);
    } else {
        while tail.len() + chunk.len() > tail_cap {
            tail.pop_front();
        }
        tail.extend(chunk);
    }
}

/// Render captured head/tail bytes to a lossy string, inserting a truncation
/// marker between them when the stream exceeded the cap.
fn finalize_output(head: &[u8], tail: &VecDeque<u8>, total: usize, cap: usize) -> String {
    let mut text = String::from_utf8_lossy(head).into_owned();
    if total > cap {
        let omitted = total - cap;
        text.push_str(&format!(
            "\n…[output truncated: {omitted} of {total} bytes omitted]\n"
        ));
    }
    let tail_bytes: Vec<u8> = tail.iter().copied().collect();
    text.push_str(&String::from_utf8_lossy(&tail_bytes));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn is_secret_name_flags_credentials_only() {
        for secret in [
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "CARGO_REGISTRY_TOKEN",
            "DB_PASSWORD",
            "NPM_CONFIG__AUTH",
            "GH_CREDENTIALS",
            "my_secret_thing",
        ] {
            assert!(is_secret_name(secret), "{secret} should be secret");
        }
        for safe in ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "LANG"] {
            assert!(!is_secret_name(safe), "{safe} should not be secret");
        }
    }

    #[test]
    fn child_env_keeps_allowlisted_and_scrubs_secrets() {
        let policy = EnvPolicy::dev_default();
        let parent = entries(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/dev"),
            ("CARGO_HOME", "/home/dev/.cargo"),
            ("CARGO_REGISTRY_TOKEN", "deadbeef"), // allowlisted prefix BUT secret
            ("OPENAI_API_KEY", "sk-xxx"),         // secret and not allowlisted
            ("RANDOM_VAR", "nope"),               // not allowlisted
        ]);

        let mut child: Vec<(String, String)> = child_env(parent.into_iter(), &policy);
        child.sort();

        let names: Vec<&str> = child.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["CARGO_HOME", "HOME", "PATH"]);
        // The secret-shaped name survived neither the allowlist nor the scrub.
        assert!(!names.contains(&"CARGO_REGISTRY_TOKEN"));
        assert!(!names.contains(&"OPENAI_API_KEY"));
        assert!(!names.contains(&"RANDOM_VAR"));
    }

    #[test]
    fn read_capped_keeps_head_and_tail_with_a_marker() {
        // 'H' head, 'M' middle (dropped), 'T' tail.
        let mut data = vec![b'H'; 20];
        data.extend(std::iter::repeat_n(b'M', 60));
        data.extend(std::iter::repeat_n(b'T', 20));
        let out = read_capped(&data[..], 20);
        assert!(out.starts_with("HHHHHHHHHH"), "head retained: {out}");
        assert!(out.ends_with("TTTTTTTTTT"), "tail retained: {out}");
        assert!(!out.contains('M'), "middle dropped: {out}");
        assert!(out.contains("80 of 100 bytes omitted"), "marker: {out}");
    }

    #[test]
    fn read_capped_passes_small_streams_through_unmarked() {
        let out = read_capped(&b"hello"[..], 1024);
        assert_eq!(out, "hello");
    }

    // The subprocess-backed checks below need a real POSIX toolchain (absolute
    // program paths, signal kill). The pure helpers above cover the cap/scrub
    // logic on every platform.
    #[cfg(unix)]
    mod posix {
        use super::*;
        use crate::sandbox::{DEFAULT_MAX_OUTPUT, NetPolicy};
        use std::path::PathBuf;

        fn policy(timeout: Duration) -> SandboxPolicy {
            SandboxPolicy {
                cwd: PathBuf::from("/"),
                fs_read: Vec::new(),
                fs_write: Vec::new(),
                net: NetPolicy::Deny,
                env: EnvPolicy::dev_default(),
                timeout,
                max_output: DEFAULT_MAX_OUTPUT,
                env_overrides: Vec::new(),
            }
        }

        #[test]
        fn runs_argv_literally_without_shell_interpretation() {
            // If a shell were involved, `&&` and `>` would be operators and the
            // output would differ; argv execution echoes them verbatim.
            let spec = CommandSpec {
                command: "/bin/echo".into(),
                args: vec!["a && b > c".into()],
            };
            let out = ProcessLauncher
                .launch(
                    &spec,
                    &policy(Duration::from_secs(10)),
                    &CancelToken::never(),
                )
                .expect("echo runs");
            assert_eq!(out.exit_code, Some(0));
            assert!(!out.timed_out);
            assert_eq!(out.stdout, "a && b > c\n");
        }

        #[test]
        fn timeout_kills_a_long_running_process() {
            let spec = CommandSpec {
                command: "/bin/sleep".into(),
                args: vec!["30".into()],
            };
            let out = ProcessLauncher
                .launch(
                    &spec,
                    &policy(Duration::from_millis(150)),
                    &CancelToken::never(),
                )
                .expect("sleep spawns");
            assert!(out.timed_out, "expected the wall-clock kill to fire");
            // Killed by signal → no exit code.
            assert_eq!(out.exit_code, None);
        }

        #[test]
        fn streaming_feeds_stdin_and_delivers_lines() {
            // `cat` echoes its stdin to stdout line-by-line: this proves the
            // streaming path both writes the task to stdin AND surfaces each
            // newline-terminated line through `on_line` (the DelegateProgress seam).
            let spec = CommandSpec {
                command: "/bin/cat".into(),
                args: Vec::new(),
            };
            let mut lines = Vec::new();
            let out = ProcessLauncher
                .launch_streaming(
                    &spec,
                    &policy(Duration::from_secs(10)),
                    "alpha\nbeta\ngamma\n",
                    &CancelToken::never(),
                    &mut |line| lines.push(line.to_string()),
                )
                .expect("cat streams");
            assert_eq!(out.exit_code, Some(0));
            assert!(!out.timed_out);
            assert_eq!(lines, vec!["alpha", "beta", "gamma"]);
            // The captured stdout mirrors the streamed lines.
            assert_eq!(out.stdout, "alpha\nbeta\ngamma\n");
        }

        #[test]
        fn cancelled_token_kills_the_process() {
            let cancel = CancelToken::new();
            cancel.cancel();
            let spec = CommandSpec {
                command: "/bin/sleep".into(),
                args: vec!["30".into()],
            };
            let out = ProcessLauncher
                .launch(&spec, &policy(Duration::from_secs(60)), &cancel)
                .expect("sleep spawns");
            // Cancellation kills the group without flagging a timeout.
            assert!(!out.timed_out);
            assert_eq!(out.exit_code, None);
        }
    }
}
