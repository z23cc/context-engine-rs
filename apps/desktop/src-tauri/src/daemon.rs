//! Selects the daemon GUI endpoint and, on desktop-local mode, supervises the
//! `nerve daemon` HTTP-transport child process.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use crate::config;
use crate::DaemonState;

/// Budget for the daemon's HTTP listener to begin accepting connections.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// PID of the running daemon, mirrored outside Tauri state so a Unix signal
/// handler — which cannot safely touch `AppHandle` — can still reap it. `0`
/// means "no daemon".
static DAEMON_PID: AtomicI32 = AtomicI32::new(0);

/// Whether this build is running on a mobile target. Mobile sandboxes cannot
/// spawn the local engine daemon, so they are remote-only.
pub(crate) fn is_mobile_target() -> bool {
    cfg!(any(target_os = "ios", target_os = "android"))
}

/// Supervisor-thread entry point: resolve the daemon endpoint and attach the
/// window, surfacing any failure into the splash page.
pub fn supervise(app: AppHandle) {
    if let Err(err) = attach(&app) {
        eprintln!("nerve-desktop: {err}");
        report_error(&app, &err);
    }
}

fn attach(app: &AppHandle) -> Result<(), String> {
    if let Some(url) = config::resolve_remote_url(app)? {
        return open_gui(app, &url);
    }
    if is_mobile_target() {
        return Err(format!(
            "mobile builds cannot spawn a local daemon; set {} or persisted `remote_url` \
             to a remote `nerve daemon --http` URL",
            config::REMOTE_URL_ENV
        ));
    }
    start_local_and_attach(app)
}

fn start_local_and_attach(app: &AppHandle) -> Result<(), String> {
    let root = config::resolve_root(app).ok_or("no workspace folder was selected")?;
    let port = free_port().map_err(|err| format!("could not find a free port: {err}"))?;
    let binary = resolve_binary()?;
    let child = Command::new(&binary)
        .arg("daemon")
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--root")
        .arg(&root)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|err| format!("failed to spawn {}: {err}", binary.display()))?;
    DAEMON_PID.store(child.id() as i32, Ordering::SeqCst);
    app.state::<DaemonState>()
        .0
        .lock()
        .map_err(|_| "daemon state lock poisoned")?
        .replace(child);
    let result = wait_ready(port).and_then(|_| {
        config::save_last_root(app, &root);
        open_gui(app, &format!("http://127.0.0.1:{port}/"))
    });
    if result.is_err() {
        shutdown(app);
    }
    result
}

fn open_gui(app: &AppHandle, url: &str) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or("main window is missing")?;
    let url = url
        .parse()
        .map_err(|err| format!("invalid daemon url `{url}`: {err}"))?;
    window.navigate(url).map_err(|err| err.to_string())
}

/// Kill the managed daemon child, if any. Idempotent and safe to call twice.
pub fn shutdown(app: &AppHandle) {
    DAEMON_PID.store(0, Ordering::SeqCst);
    let Some(state) = app.try_state::<DaemonState>() else {
        return;
    };
    // Take the child out while holding the lock only for that statement, then
    // kill it after the guard (a temporary) has been dropped.
    let child = state.0.lock().ok().and_then(|mut guard| guard.take());
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Ask the OS for an unused loopback TCP port by binding to port 0.
fn free_port() -> io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

/// Poll the loopback port until the daemon's HTTP listener accepts a connection.
fn wait_ready(port: u16) -> Result<(), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "daemon did not start on 127.0.0.1:{port} within {}s",
                READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Locate the `nerve` binary: an explicit override, a sidecar shipped next to
/// the app, or — in development — the engine workspace's build output.
fn resolve_binary() -> Result<PathBuf, String> {
    let name = if cfg!(windows) { "nerve.exe" } else { "nerve" };
    if let Some(path) = env_override() {
        return Ok(path);
    }
    if let Some(path) = sidecar_binary(name) {
        return Ok(path);
    }
    if let Some(path) = dev_binary(name) {
        return Ok(path);
    }
    Err(format!(
        "could not locate the `{name}` binary. Set NERVE_BIN to its path, or build it with \
         `cargo build -p nerve-workstation --bin nerve` in the engine workspace."
    ))
}

fn env_override() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var("NERVE_BIN").ok()?.trim());
    path.is_file().then_some(path)
}

/// A `nerve` binary shipped alongside the app executable (Tauri sidecar layout),
/// including the macOS `.app` `Resources` directory.
fn sidecar_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let beside = dir.join(name);
    if beside.is_file() {
        return Some(beside);
    }
    #[cfg(target_os = "macos")]
    {
        let resources = dir.parent()?.join("Resources").join(name);
        if resources.is_file() {
            return Some(resources);
        }
    }
    None
}

/// The engine workspace build output, relative to this crate at
/// `<repo>/apps/desktop/src-tauri`.
fn dev_binary(name: &str) -> Option<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest.parent()?.parent()?.parent()?;
    ["debug", "release"]
        .into_iter()
        .map(|profile| repo_root.join("target").join(profile).join(name))
        .find(|candidate| candidate.is_file())
}

fn report_error(app: &AppHandle, message: &str) {
    if let Some(window) = app.get_webview_window("main") {
        let escaped = message
            .replace('\\', "\\\\")
            .replace('`', "\\`")
            .replace("${", "\\${");
        let _ = window.eval(format!(
            "window.__nerveError && window.__nerveError(`{escaped}`)"
        ));
    }
}

/// Install async-signal-safe SIGINT/SIGTERM handlers that reap the daemon child
/// before exiting. Tao does not forward these signals to Tauri's `RunEvent`, so
/// without this a `Ctrl-C` on `tauri dev` (SIGINT) or a `kill` (SIGTERM) would
/// orphan the daemon. GUI quits (Cmd-Q, window close) still flow through
/// [`shutdown`] via `RunEvent`.
#[cfg(unix)]
pub fn install_exit_signal_handler() {
    let handler = reap_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: registering a handler that only calls async-signal-safe functions.
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
}

#[cfg(unix)]
extern "C" fn reap_and_exit(_signal: libc::c_int) {
    // Only async-signal-safe operations are allowed here: an atomic load,
    // `kill(2)`, and `_exit(2)`.
    let pid = DAEMON_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: `kill` is async-signal-safe; a dead PID just yields ESRCH.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
    // SAFETY: `_exit` is async-signal-safe and terminates without running
    // (non-reentrant) at-exit handlers.
    unsafe { libc::_exit(0) };
}

#[cfg(not(unix))]
pub fn install_exit_signal_handler() {}
