//! Thin native shell for the Nerve Workstation daemon GUI.
//!
//! Desktop builds are local-first: by default they spawn
//! `nerve daemon --http 127.0.0.1:<port> --root <dir>` as a managed child, wait
//! for the HTTP transport, and point the window at the daemon-served GUI.
//! Mobile builds are remote-only: they never spawn a daemon and instead navigate
//! to a configured remote daemon URL such as a desktop/server reached over
//! Tailscale. All intelligence stays in the engine and its existing HTTP
//! transport; this crate only supervises or selects the GUI endpoint.

mod config;
mod daemon;

use std::process::Child;
use std::sync::Mutex;

use tauri::RunEvent;

/// Managed state: the running desktop daemon child, killed on exit.
#[derive(Default)]
struct DaemonState(Mutex<Option<Child>>);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Reap the daemon on Ctrl-C / kill (signals tao does not surface to RunEvent).
    // Mobile builds never spawn a daemon, so no process signal hook is needed there.
    if !daemon::is_mobile_target() {
        daemon::install_exit_signal_handler();
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(DaemonState::default())
        .setup(|app| {
            // Supervise on a background thread so `setup` returns immediately (the
            // splash paints while the daemon boots) and any blocking native folder
            // picker runs off the main thread, as its API requires.
            let handle = app.handle().clone();
            std::thread::spawn(move || daemon::supervise(handle));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build the nerve-desktop application")
        .run(|handle, event| {
            if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
                daemon::shutdown(handle);
            }
        });
}
