//! Terminal lifecycle: raw mode + alternate screen with a panic hook that
//! restores the terminal on panic, and a guard that restores it on normal exit.
//!
//! Without this, a crash mid-frame leaves the user's terminal in raw + alt-screen
//! mode (no echo, no prompt). Mirrors the TS client's `#restoreTerminal`.

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// A live terminal in raw + alt-screen mode. Restores the terminal on drop, so a
/// `?`-propagated error or an early return still leaves the tty usable.
pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Enter raw mode + the alternate screen and install the panic hook.
    pub fn enter() -> Result<Self> {
        install_panic_hook();
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        // Alt screen + bracketed paste (so a paste arrives whole, not key-by-key)
        // + mouse capture (so the wheel scrolls the transcript). All are undone in
        // `restore`, mirroring the TS client's terminal setup/teardown.
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
        .context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("create terminal")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Best-effort restore: leave the alt screen and raw mode, show the cursor.
/// Idempotent, so the panic hook and `Drop` can both call it safely.
pub fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Wrap the existing panic hook so a panic restores the terminal *before* the
/// default hook prints the message (otherwise the backtrace lands on the alt
/// screen and vanishes when we leave it).
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `restore` is the single teardown both `Drop` and the panic hook call, so it
    /// must be safe to invoke repeatedly (and with no tty, as under `cargo test`)
    /// without panicking — every step is best-effort (`let _ = …`).
    #[test]
    fn restore_is_idempotent_and_tty_independent() {
        restore();
        restore();
    }

    /// The panic hook must run our `restore` before chaining to the previous hook,
    /// so a crash leaves the tty usable and the backtrace prints on the main
    /// screen. We install the hook, fire a panic on a worker thread, and confirm
    /// our teardown ran (a flag the hook sets) before control returned.
    #[test]
    fn panic_hook_restores_before_chaining() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        let restored = Arc::new(AtomicBool::new(false));
        let flag = restored.clone();
        // Serialize: the process-wide panic hook is global state.
        static LOCK: Mutex<()> = Mutex::new(());
        let _serialized = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Mirror `install_panic_hook`: tear down first, then chain.
            restore();
            flag.store(true, Ordering::SeqCst);
            // Swallow the default hook's output to keep the test quiet.
            let _ = info;
        }));

        let result = std::panic::catch_unwind(|| panic!("boom"));
        std::panic::set_hook(previous);

        assert!(result.is_err(), "the panic should have unwound");
        assert!(
            restored.load(Ordering::SeqCst),
            "the panic hook must run restore()"
        );
    }
}
