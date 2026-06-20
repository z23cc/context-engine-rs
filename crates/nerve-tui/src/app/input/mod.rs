//! Key dispatch, the slash-command palette, and the command handlers.
//!
//! Ports `app.ts`'s `#onKey`, `#handlePaletteKey`/`#completePalette`, `#onSubmit`,
//! `#onCommand` (+ `#switchModel`/`#setMode`/`#onModeCommand`/`#newSession`/
//! `#listModels`), and the history/scroll keybindings, onto crossterm
//! [`KeyEvent`]s. Pure decisions (what a key does) live in small helpers so they
//! stay unit-testable; the IO (sending jobs) is on [`Shell`].
//!
//! Key map (input mode):
//!   Enter            submit (gated on running → hint)
//!   Alt/Shift-Enter  insert newline
//!   Up/Down          history (first row) · palette nav · transcript scroll
//!   PgUp/PgDn        scroll the transcript
//!   Left/Right/Home/End  cursor movement
//!   Backspace        delete grapheme · Ctrl-U kill line · Ctrl-W kill word
//!   Ctrl-O           toggle tool-output expansion
//!   Ctrl-C           interrupt if running, else quit · Ctrl-D quit · Ctrl-L redraw
//!   Tab              complete the selected palette command
//!   mouse wheel      scroll (handled in the event loop)

mod delegate;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nerve_runtime::{ApprovalMode, RuntimeCommand, SessionApprovalDecision};
use serde_json::Value;
use std::collections::BTreeMap;

use super::Shell;
use super::state::{ApprovalState, Mode, Tone};
use crate::ui::commands::{
    COMMANDS, CommandSpec, HELP_TEXT, SlashCommand, approval_mode_label, format_models,
    match_commands, parse_approval_mode, parse_command, provider_models_tool,
};
use crate::ui::theme::THEMES;
use delegate::{SubmitRoute, submit_route};

/// One-screen worth of rows to jump on PgUp/PgDn (a generous default; the loop
/// clamps the scroll to the transcript length at render time).
const PAGE_ROWS: usize = 20;
/// Scroll step for the mouse wheel / arrow keys when not navigating history.
const SCROLL_STEP: usize = 3;

impl Shell {
    /// Handle one key. Returns `true` when the loop should exit. Async because
    /// command/submit paths send jobs to the daemon.
    pub(crate) async fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if self.state.mode == Mode::Approval {
            // Approval keys never reach the editor; a non-decision key keeps the
            // prompt up. Mirrors the TS `#onApprovalKey`.
            self.on_approval_key(key).await;
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl-O toggles tool expansion regardless of the palette (matches the TS
        // early return before palette handling).
        if ctrl && key.code == KeyCode::Char('o') {
            self.state.expand_tools = !self.state.expand_tools;
            return false;
        }
        // While a bare `/word` palette is open, arrows/Tab/Enter drive it first.
        let palette = match_commands(self.state.editor.value());
        if !palette.is_empty() && self.handle_palette_key(key, &palette) {
            return false;
        }
        self.state.hint.clear();
        self.dispatch(key, ctrl).await
    }

    /// The main (non-palette) key switch. Returns `true` to exit.
    async fn dispatch(&mut self, key: KeyEvent, ctrl: bool) -> bool {
        match key.code {
            KeyCode::Char('d') if ctrl => return true,
            KeyCode::Char('c') if ctrl => return self.on_ctrl_c().await,
            KeyCode::Char('l') if ctrl => {} // redraw: the loop re-renders anyway
            KeyCode::Char('u') if ctrl => self.state.editor.kill_line(),
            KeyCode::Char('w') if ctrl => self.state.editor.kill_word(),
            KeyCode::Enter if newline_chord(key.modifiers) => self.state.editor.insert("\n"),
            KeyCode::Enter => self.on_submit().await,
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Backspace => {
                self.state.editor.backspace();
                self.state.palette_index = 0;
            }
            KeyCode::Left => self.state.editor.left(),
            KeyCode::Right => self.state.editor.right(),
            KeyCode::Home => self.state.editor.home(),
            KeyCode::End => self.state.editor.end(),
            KeyCode::Up => self.on_up(),
            KeyCode::Down => self.on_down(),
            KeyCode::PageUp => self.state.scroll += PAGE_ROWS,
            KeyCode::PageDown => {
                self.state.scroll = self.state.scroll.saturating_sub(PAGE_ROWS);
            }
            _ => {}
        }
        false
    }

    /// Insert a typed char, resetting the palette selection (the editor resets
    /// history browsing). Mirrors the TS `#insert`.
    fn insert_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.state.editor.insert(c.encode_utf8(&mut buf));
        self.state.palette_index = 0;
    }

    /// Insert a pasted block verbatim (no submit). Mirrors the TS `paste` key.
    pub(crate) fn handle_paste(&mut self, text: &str) {
        if self.state.mode == Mode::Approval {
            return;
        }
        self.state.editor.insert(text);
        self.state.palette_index = 0;
    }

    /// Handle a key while the approval modal is up. A decision key sends a
    /// `session.respond`, clears the modal, returns to input, and pushes a notice;
    /// other keys are ignored so the prompt persists. Ports the TS `#onApprovalKey`.
    async fn on_approval_key(&mut self, key: KeyEvent) {
        let Some(decision) = approval_decision_for_key(key) else {
            return;
        };
        // Take the pending approval; without one there is nothing to answer.
        let Some(approval) = self.state.approval.take() else {
            self.state.mode = Mode::Input;
            return;
        };
        self.state.mode = Mode::Input;
        let tool = approval.tool.clone();
        self.send(respond_command(approval, decision)).await;
        self.state
            .note(format!("{} {}", decision_verb(decision), tool));
    }

    /// Ctrl-C: interrupt an in-flight turn, else quit. Returns `true` to exit.
    async fn on_ctrl_c(&mut self) -> bool {
        if self.state.running && self.state.session_id.is_some() {
            self.interrupt().await;
            false
        } else {
            true
        }
    }

    /// Up: history when on the first editor row (matches `app.ts`), else scroll.
    fn on_up(&mut self) {
        if self.state.editor.cursor_on_first_row() {
            self.state.editor.history_prev();
            self.state.palette_index = 0;
        } else {
            self.state.scroll += SCROLL_STEP;
        }
    }

    /// Down: history when browsing, else scroll toward the tail.
    fn on_down(&mut self) {
        if self.state.editor.cursor_on_first_row() {
            self.state.editor.history_next();
            self.state.palette_index = 0;
        } else {
            self.state.scroll = self.state.scroll.saturating_sub(SCROLL_STEP);
        }
    }

    /// Palette navigation/completion. Returns `true` if the key was consumed.
    /// Ports `#handlePaletteKey`.
    fn handle_palette_key(&mut self, key: KeyEvent, palette: &[CommandSpec]) -> bool {
        let len = palette.len();
        match key.code {
            KeyCode::Up => {
                self.state.palette_index = (self.state.palette_index + len - 1) % len;
                true
            }
            KeyCode::Down => {
                self.state.palette_index = (self.state.palette_index + 1) % len;
                true
            }
            KeyCode::Tab => {
                self.complete_palette(palette);
                true
            }
            KeyCode::Enter => {
                let sel = palette[self.state.palette_index % len];
                // Not yet the exact command → complete it; else fall through to
                // submit so `/quit` etc. run.
                if self.state.editor.value() != format!("/{}", sel.name) {
                    self.complete_palette(palette);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Replace the input with the selected `/command ` (trailing space). Ports
    /// `#completePalette`.
    fn complete_palette(&mut self, palette: &[CommandSpec]) {
        let sel = palette[self.state.palette_index % palette.len()];
        self.state.editor.set_value(format!("/{} ", sel.name));
        self.state.palette_index = 0;
    }

    /// Enter: submit the input as a message or a slash command. Ports `#onSubmit`.
    async fn on_submit(&mut self) {
        let text = self.state.editor.clear();
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.state.editor.push_history(&text);
        self.state.palette_index = 0;
        if let Some(command) = parse_command(&text) {
            self.on_command(command).await;
            return;
        }
        self.send_message(text).await;
    }

    /// Send a user message. When a delegate session is active, plain input steers
    /// that session (`delegate.steer`) instead of messaging the chat
    /// (`session.message`) (DA-5d); the routing + guards are decided purely by
    /// [`submit_route`], so this method only applies the side effects.
    async fn send_message(&mut self, text: String) {
        match submit_route(&self.state, text) {
            SubmitRoute::Hint(hint) => self.state.hint = hint,
            SubmitRoute::Send(command, echo) => {
                self.state.push_user(&echo);
                self.state.running = true;
                self.state.scroll = 0;
                self.state.end_stream();
                self.send(*command).await;
            }
        }
    }

    /// Dispatch a parsed slash command. Ports `#onCommand` (+ friendly aliases).
    async fn on_command(&mut self, command: SlashCommand) {
        let SlashCommand { cmd, rest } = command;
        match cmd.as_str() {
            "quit" | "exit" => self.state.note("(quit) — press Ctrl-D to exit"),
            "help" => self.state.push_notice(Tone::Info, HELP_TEXT),
            "model" => self.cmd_model(&rest),
            "provider" => self.cmd_provider(&rest),
            "models" => self.list_models().await,
            "mode" => self.cmd_mode(&rest),
            "yolo" => self.set_mode(ApprovalMode::Yolo).await,
            "write" => self.set_mode(ApprovalMode::Write).await,
            "ask" => self.set_mode(ApprovalMode::AlwaysAsk).await,
            "delegate" => self.cmd_delegate(&rest).await,
            "done" | "close" => self.cmd_done().await,
            "new" | "reset" => self.new_session().await,
            "login" => self.cmd_login(&rest),
            "theme" => self.cmd_theme(),
            other => self.state.hint = format!("unknown command: /{other} — try /help"),
        }
    }

    /// `/model [id]`: bare shows the current model; an arg switches it.
    fn cmd_model(&mut self, rest: &str) {
        if rest.is_empty() {
            self.state.hint = format!(
                "current: {}/{} — usage: /model <id>",
                self.state.provider, self.state.model
            );
        } else {
            let provider = self.state.provider.clone();
            self.switch_model(provider, rest.to_string());
        }
    }

    /// `/provider <name> [model]`: switch provider (keeping the model if omitted).
    fn cmd_provider(&mut self, rest: &str) {
        if rest.is_empty() {
            self.state.hint = "usage: /provider <name> [model]".to_string();
            return;
        }
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or_default().to_string();
        let model = parts
            .next()
            .map_or_else(|| self.state.model.clone(), str::to_string);
        self.switch_model(name, model);
    }

    /// Switch the live session's provider/model in place. Ports `#switchModel`.
    fn switch_model(&mut self, provider: String, model: String) {
        let Some(session_id) = self.state.session_id.clone() else {
            self.state.hint = "no active session yet".to_string();
            return;
        };
        self.state.provider = provider.clone();
        self.state.model = model.clone();
        let command = RuntimeCommand::SessionSetModel {
            session_id,
            provider: Some(provider.clone()),
            model: model.clone(),
        };
        self.state.note(format!("switched to {provider}/{model}"));
        self.spawn_send(command);
    }

    /// `/mode [always-ask|write|yolo]`: bare shows current; an arg sets it.
    /// Ports `#onModeCommand`.
    fn cmd_mode(&mut self, rest: &str) {
        if rest.is_empty() {
            self.state.hint = format!(
                "mode: {} — usage: /mode always-ask|write|yolo",
                approval_mode_label(self.state.approval_mode)
            );
            return;
        }
        match parse_approval_mode(rest) {
            Some(mode) => self.spawn_set_mode(mode),
            None => self.state.hint = format!("unknown mode: {rest} — try always-ask|write|yolo"),
        }
    }

    /// Set the approval mode and push it to the session. Ports `#setMode`.
    async fn set_mode(&mut self, mode: ApprovalMode) {
        self.state.approval_mode = mode;
        self.state.hint = format!("approval mode: {}", approval_mode_label(mode));
        if let Some(session_id) = self.state.session_id.clone() {
            self.send(RuntimeCommand::SessionSetMode { session_id, mode })
                .await;
        }
    }

    /// Sync variant of [`set_mode`](Self::set_mode) for the non-async `/mode` path.
    fn spawn_set_mode(&mut self, mode: ApprovalMode) {
        self.state.approval_mode = mode;
        self.state.hint = format!("approval mode: {}", approval_mode_label(mode));
        if let Some(session_id) = self.state.session_id.clone() {
            self.spawn_send(RuntimeCommand::SessionSetMode { session_id, mode });
        }
    }

    /// `/login [provider]`: print the auth instruction. Ports the TS `login` arm.
    fn cmd_login(&mut self, rest: &str) {
        let who = if rest.is_empty() {
            "claude|chatgpt|xai"
        } else {
            rest
        };
        self.state.push_notice(
            Tone::Info,
            format!("authenticate with:  nerve agent login --provider {who}"),
        );
    }

    /// `/theme`: cycle the accent color. Ports the TS `theme` arm.
    fn cmd_theme(&mut self) {
        self.state.theme_index = (self.state.theme_index + 1) % THEMES.len();
        self.state.hint = format!("theme: {}", THEMES[self.state.theme_index].name);
    }

    /// `/new`: close the old session, clear the transcript+meters, start fresh.
    /// Ports `#newSession`.
    async fn new_session(&mut self) {
        let previous = self.state.session_id.take();
        self.state.blocks.clear();
        self.state.reset_meters();
        self.state.end_stream();
        self.state.scroll = 0;
        if let Some(previous) = previous {
            self.send(RuntimeCommand::SessionClose {
                session_id: previous,
            })
            .await;
        }
        self.state.note(format!(
            "new session · {}/{}",
            self.state.provider, self.state.model
        ));
        let command =
            self.session_start_command(self.state.provider.clone(), self.state.model.clone());
        self.send(command).await;
    }

    /// `/models`: run the provider's model-list tool and print the result. Ports
    /// `#listModels`.
    async fn list_models(&mut self) {
        let Some(tool) = provider_models_tool(&self.state.provider) else {
            self.state.hint = format!("no model list for {}", self.state.provider);
            return;
        };
        self.state.note(format!("fetching models ({tool})…"));
        let command = RuntimeCommand::ToolCall {
            name: tool.to_string(),
            arguments: BTreeMap::new(),
        };
        match self.client.run_job(command, None).await {
            Ok(result) => {
                self.state.hint.clear();
                self.state
                    .push_notice(Tone::Info, format!("models:\n{}", format_models(&result)));
            }
            Err(err) => self.state.push_notice(Tone::Error, err.to_string()),
        }
    }

    /// Ctrl-C interrupt of the in-flight turn (a no-op when idle).
    async fn interrupt(&mut self) {
        let Some(session_id) = self.state.session_id.clone() else {
            return;
        };
        if !self.state.running {
            return;
        }
        self.state.hint = "interrupting…".to_string();
        self.send(RuntimeCommand::SessionInterrupt { session_id })
            .await;
    }

    /// Send a command, surfacing a transport error as a red notice.
    async fn send(&mut self, command: RuntimeCommand) {
        if let Err(err) = self.client.start_job(command, None).await {
            self.state.push_notice(Tone::Error, err.to_string());
        }
    }

    /// Fire-and-forget a command from a sync handler (model/mode switches). Errors
    /// are dropped — the next `send` surfaces transport failures.
    fn spawn_send(&self, command: RuntimeCommand) {
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.start_job(command, None).await;
        });
    }
}

/// Whether an Enter chord means "newline" (Alt or Shift held), not "submit".
/// Ports the TS `alt-enter` decode (plus Shift-Enter, which some terminals send).
#[must_use]
fn newline_chord(modifiers: KeyModifiers) -> bool {
    modifiers.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
}

/// Map an approval keypress to a [`SessionApprovalDecision`], or `None` if the key
/// isn't a decision (so the prompt stays up). Case matters: lowercase = once,
/// uppercase = "always". `Esc` cancels (deny once). Ports `approvalDecisionForKey`:
///   a/y → allow · A → allow_always · d/n → deny · D → deny_always · Esc → deny
#[must_use]
fn approval_decision_for_key(key: KeyEvent) -> Option<SessionApprovalDecision> {
    use SessionApprovalDecision::{Allow, AllowAlways, Deny, DenyAlways};
    if key.code == KeyCode::Esc {
        return Some(Deny);
    }
    // Only bare character keys are decisions; a control chord (Ctrl-A etc.) is not
    // — the TS only inspects `key.type === "char"`, never the ctrl-decoded keys.
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    match key.code {
        KeyCode::Char('a' | 'y') => Some(Allow),
        KeyCode::Char('A') => Some(AllowAlways),
        KeyCode::Char('d' | 'n') => Some(Deny),
        KeyCode::Char('D') => Some(DenyAlways),
        _ => None,
    }
}

/// Build the `session.respond` command answering a pending approval. Pure so the
/// session/request routing is testable without a live client. Ports the TS
/// `#send({ kind: "session.respond", … })`.
#[must_use]
fn respond_command(approval: ApprovalState, decision: SessionApprovalDecision) -> RuntimeCommand {
    RuntimeCommand::SessionRespond {
        session_id: approval.session_id,
        request_id: approval.request_id,
        decision,
    }
}

/// Past-tense verb for a decision notice. Ports the TS `decisionVerb`.
#[must_use]
fn decision_verb(decision: SessionApprovalDecision) -> &'static str {
    match decision {
        SessionApprovalDecision::Allow => "allowed",
        SessionApprovalDecision::AllowAlways => "always-allowed",
        SessionApprovalDecision::Deny => "denied",
        SessionApprovalDecision::DenyAlways => "always-denied",
    }
}

/// Compact-JSON a value for a notice/header (a string is passed through). Mirrors
/// the TS `safeJson`. Currently used by tests; kept here next to its siblings.
#[cfg_attr(not(test), allow(dead_code))]
#[must_use]
fn safe_json(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The slash-command palette specs (re-exported for tests asserting the list).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn palette_specs() -> &'static [CommandSpec] {
    COMMANDS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn newline_chord_detects_alt_and_shift() {
        assert!(newline_chord(KeyModifiers::ALT));
        assert!(newline_chord(KeyModifiers::SHIFT));
        assert!(!newline_chord(KeyModifiers::NONE));
        assert!(!newline_chord(KeyModifiers::CONTROL));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn approval_decision_key_map() {
        use SessionApprovalDecision::{Allow, AllowAlways, Deny, DenyAlways};
        // lowercase = once, uppercase = "always"; a/y allow, d/n deny, Esc denies.
        assert_eq!(
            approval_decision_for_key(key(KeyCode::Char('a'))),
            Some(Allow)
        );
        assert_eq!(
            approval_decision_for_key(key(KeyCode::Char('y'))),
            Some(Allow)
        );
        assert_eq!(
            approval_decision_for_key(key(KeyCode::Char('A'))),
            Some(AllowAlways)
        );
        assert_eq!(
            approval_decision_for_key(key(KeyCode::Char('d'))),
            Some(Deny)
        );
        assert_eq!(
            approval_decision_for_key(key(KeyCode::Char('n'))),
            Some(Deny)
        );
        assert_eq!(
            approval_decision_for_key(key(KeyCode::Char('D'))),
            Some(DenyAlways)
        );
        assert_eq!(approval_decision_for_key(key(KeyCode::Esc)), Some(Deny));
    }

    #[test]
    fn approval_decision_ignores_non_decision_keys() {
        // Arrows / Enter / unrelated chars keep the prompt up (return None).
        assert_eq!(approval_decision_for_key(key(KeyCode::Up)), None);
        assert_eq!(approval_decision_for_key(key(KeyCode::Enter)), None);
        assert_eq!(approval_decision_for_key(key(KeyCode::Char('z'))), None);
        // A control chord is not a decision (Ctrl-A must not allow).
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(approval_decision_for_key(ctrl_a), None);
    }

    #[test]
    fn decision_verb_is_past_tense() {
        use SessionApprovalDecision::{Allow, AllowAlways, Deny, DenyAlways};
        assert_eq!(decision_verb(Allow), "allowed");
        assert_eq!(decision_verb(AllowAlways), "always-allowed");
        assert_eq!(decision_verb(Deny), "denied");
        assert_eq!(decision_verb(DenyAlways), "always-denied");
    }

    #[test]
    fn respond_command_routes_session_and_request() {
        use nerve_runtime::RiskTier;
        let approval = ApprovalState {
            tool: "edit".into(),
            args: "{}".into(),
            request_id: "req-7".into(),
            session_id: "sess-3".into(),
            tier: RiskTier::Edit,
            preview: String::new(),
        };
        let command = respond_command(approval, SessionApprovalDecision::AllowAlways);
        match command {
            RuntimeCommand::SessionRespond {
                session_id,
                request_id,
                decision,
            } => {
                assert_eq!(session_id, "sess-3");
                assert_eq!(request_id, "req-7");
                assert_eq!(decision, SessionApprovalDecision::AllowAlways);
            }
            other => panic!("expected SessionRespond, got {other:?}"),
        }
    }

    #[test]
    fn respond_command_uses_the_events_session_id_for_a_delegate_approval() {
        use nerve_runtime::RiskTier;
        // A delegate tool's approval carries the delegate-job id as session_id (the
        // `ApprovalRequested` event populates it). The respond must route there, not
        // to the chat session — so the delegated agent gets the decision (DA-5d §3).
        let approval = ApprovalState {
            tool: "edit".into(),
            args: "{}".into(),
            request_id: "req-delegate-1".into(),
            session_id: "delegate-job-42".into(),
            tier: RiskTier::Edit,
            preview: String::new(),
        };
        let command = respond_command(approval, SessionApprovalDecision::Allow);
        match command {
            RuntimeCommand::SessionRespond {
                session_id,
                request_id,
                decision,
            } => {
                assert_eq!(session_id, "delegate-job-42");
                assert_eq!(request_id, "req-delegate-1");
                assert_eq!(decision, SessionApprovalDecision::Allow);
            }
            other => panic!("expected SessionRespond, got {other:?}"),
        }
    }

    #[test]
    fn safe_json_unwraps_strings() {
        assert_eq!(safe_json(&json!("hi")), "hi");
        assert_eq!(safe_json(&json!({ "a": 1 })), r#"{"a":1}"#);
    }

    #[test]
    fn palette_specs_are_the_command_list() {
        assert!(palette_specs().iter().any(|c| c.name == "model"));
        assert!(palette_specs().iter().any(|c| c.name == "quit"));
    }
}
