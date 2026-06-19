//! The minimal ratatui shell: connect → handshake → `session.start`, then a
//! `tokio::select!` loop multiplexing keyboard input, protocol events, and a
//! tick. Enter sends `session.message`; Ctrl-C interrupts the turn; Ctrl-D quits.
//!
//! Deliberately minimal — T2/T3/T4 add rich rendering, an editor, slash
//! commands, and the approval modal. The interactive LLM path needs provider
//! credentials, so it is exercised by hand, not in CI; the protocol client and
//! the render path are what the tests cover.

mod events;
pub mod state;
mod terminal;

use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use nerve_runtime::{RuntimeCommand, RuntimeEvent};
use tokio::sync::broadcast;

use crate::protocol::{DaemonSpec, NerveClient};
use events::apply_event;
use state::State;
use terminal::TerminalGuard;

/// Run the interactive shell against a daemon spawned from `spec`, starting a
/// session with `provider`/`model`.
pub async fn run(spec: DaemonSpec, provider: String, model: String) -> Result<()> {
    let (client, events) = NerveClient::connect(spec).await?;
    let mut shell = Shell::new(client, events, State::new(provider.clone(), model.clone()));
    shell.startup(provider, model).await;
    let result = shell.event_loop().await;
    shell.client.shutdown().await;
    result
}

struct Shell {
    client: NerveClient,
    events: broadcast::Receiver<RuntimeEvent>,
    state: State,
}

impl Shell {
    fn new(client: NerveClient, events: broadcast::Receiver<RuntimeEvent>, state: State) -> Self {
        Self {
            client,
            events,
            state,
        }
    }

    /// Populate the tool count and open the session.
    async fn startup(&mut self, provider: String, model: String) {
        self.state.tools = self.client.list_tools().await.map(|t| t.len()).unwrap_or(0);
        self.state.note("connecting…");
        let command = RuntimeCommand::SessionStart {
            workspace: None,
            provider,
            model,
            system_prompt: None,
            agent: None,
            resume: None,
            max_turns: None,
            temperature: None,
            reasoning_effort: None,
            tool_filter: None,
        };
        if let Err(err) = self.client.start_job(command, None).await {
            self.state.note(format!("session.start failed: {err}"));
        }
    }

    /// The main multiplexed loop. Returns when the user quits (Ctrl-D).
    async fn event_loop(&mut self) -> Result<()> {
        let mut guard = TerminalGuard::enter()?;
        let mut keys = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(120));
        self.draw(&mut guard)?;
        loop {
            let mut dirty = false;
            tokio::select! {
                maybe_key = keys.next() => match maybe_key {
                    Some(Ok(Event::Key(key))) => {
                        if self.handle_key(key).await {
                            return Ok(());
                        }
                        dirty = true;
                    }
                    Some(Ok(Event::Resize(_, _))) => dirty = true,
                    Some(Err(_)) | None => return Ok(()),
                    _ => {}
                },
                event = self.events.recv() => if let Some(redraw) = self.on_event(event) {
                    dirty = redraw;
                },
                _ = tick.tick() => dirty = self.state.running,
            }
            if dirty {
                self.draw(&mut guard)?;
            }
        }
    }

    /// Fold one broadcast result into state. `None` means the stream closed and
    /// nothing changed; `Some(redraw)` reports whether to re-render.
    fn on_event(
        &mut self,
        event: Result<RuntimeEvent, broadcast::error::RecvError>,
    ) -> Option<bool> {
        match event {
            Ok(event) => Some(apply_event(&mut self.state, &event)),
            Err(broadcast::error::RecvError::Lagged(_)) => Some(false),
            Err(broadcast::error::RecvError::Closed) => {
                self.state.note("daemon disconnected");
                Some(true)
            }
        }
    }

    /// Handle a key. Returns `true` if the loop should exit.
    async fn handle_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('d') if ctrl => return true,
            KeyCode::Char('c') if ctrl => self.interrupt().await,
            KeyCode::Enter => self.submit().await,
            KeyCode::Backspace => {
                self.state.input.pop();
            }
            KeyCode::Char(c) => self.state.input.push(c),
            _ => {}
        }
        false
    }

    /// Submit the current input as a `session.message` (no-op when empty/busy).
    async fn submit(&mut self) {
        let text = std::mem::take(&mut self.state.input).trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(session_id) = self.state.session_id.clone() else {
            self.state.hint = "session not ready yet".to_string();
            return;
        };
        if self.state.running {
            self.state.hint = "still working — Ctrl-C to interrupt".to_string();
            self.state.input = text;
            return;
        }
        self.state.hint.clear();
        self.state.push_user(&text);
        self.state.running = true;
        self.state.end_stream();
        let command = RuntimeCommand::SessionMessage { session_id, text };
        if let Err(err) = self.client.start_job(command, None).await {
            self.state.running = false;
            self.state.note(format!("send failed: {err}"));
        }
    }

    /// Ctrl-C: interrupt the in-flight turn (a no-op when idle).
    async fn interrupt(&mut self) {
        let Some(session_id) = self.state.session_id.clone() else {
            return;
        };
        if !self.state.running {
            return;
        }
        self.state.hint = "interrupting…".to_string();
        let command = RuntimeCommand::SessionInterrupt { session_id };
        if let Err(err) = self.client.start_job(command, None).await {
            self.state.note(format!("interrupt failed: {err}"));
        }
    }

    fn draw(&mut self, guard: &mut TerminalGuard) -> Result<()> {
        guard
            .terminal
            .draw(|frame| state::render(frame, &self.state))?;
        Ok(())
    }
}
