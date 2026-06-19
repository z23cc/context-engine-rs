//! Terminal lifecycle: raw mode + alternate screen with a panic hook that
//! restores the terminal on panic, and a guard that restores it on normal exit.
//!
//! Without this, a crash mid-frame leaves the user's terminal in raw + alt-screen
//! mode (no echo, no prompt). Mirrors the TS client's `#restoreTerminal`.

use std::io::{self, Stdout};

use anyhow::{Context, Result};
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
        execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
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
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = execute!(io::stdout(), crossterm::cursor::Show);
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
