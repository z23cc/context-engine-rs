//! Pure UI state + frame rendering for the minimal shell.
//!
//! The render path is a pure function of [`State`] → ratatui widgets, so it is
//! testable against a `TestBackend` with no terminal. T2/T3 replace this with the
//! rich transcript/markdown renderer; for T1 it proves the streaming wiring.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Borders, Paragraph, Wrap};

/// One rendered transcript entry. T1 keeps three flat kinds; richer block types
/// (tools, diffs, approvals) arrive in later waves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A line the user submitted.
    User(String),
    /// Streaming assistant text (appended in place as deltas arrive).
    Assistant(String),
    /// A client-side notice (connection status, errors, hints).
    Notice(String),
}

/// The whole shell state. Mutated by the event loop; rendered purely.
#[derive(Debug, Clone)]
pub struct State {
    pub provider: String,
    pub model: String,
    pub tools: usize,
    pub blocks: Vec<Block>,
    pub input: String,
    /// True while a turn is in flight (drives the status line).
    pub running: bool,
    /// One-shot status hint (e.g. "interrupting…"); cleared on next input.
    pub hint: String,
    pub session_id: Option<String>,
    /// Index of the assistant block currently being streamed into, if any.
    assistant: Option<usize>,
}

impl State {
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            tools: 0,
            blocks: Vec::new(),
            input: String::new(),
            running: false,
            hint: String::new(),
            session_id: None,
            assistant: None,
        }
    }

    /// Push a client-side notice block.
    pub fn note(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::Notice(text.into()));
        self.assistant = None;
    }

    /// Push a user message block.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::User(text.into()));
        self.assistant = None;
    }

    /// Append a streaming assistant delta, coalescing into the current assistant
    /// block when one is open (mirrors the TS `#appendText`).
    pub fn append_assistant(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(index) = self.assistant
            && let Some(Block::Assistant(text)) = self.blocks.get_mut(index)
        {
            text.push_str(delta);
            return;
        }
        self.blocks.push(Block::Assistant(delta.to_string()));
        self.assistant = Some(self.blocks.len() - 1);
    }

    /// End the current streaming run so the next delta starts a fresh block.
    pub fn end_stream(&mut self) {
        self.assistant = None;
    }
}

/// Render the whole frame. Pure w.r.t. `state`; safe to drive from a
/// `TestBackend`. Layout: header / transcript / status / input.
pub fn render(frame: &mut Frame, state: &State) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);
    frame.render_widget(header(state), chunks[0]);
    frame.render_widget(transcript(state), chunks[1]);
    frame.render_widget(status(state), chunks[2]);
    frame.render_widget(input(state), chunks[3]);
}

fn header(state: &State) -> Paragraph<'_> {
    let text = format!(
        " Nerve  {}/{}  · {} tools",
        state.provider, state.model, state.tools
    );
    Paragraph::new(text).style(Style::default().add_modifier(Modifier::REVERSED))
}

fn transcript(state: &State) -> Paragraph<'_> {
    let lines: Vec<Line<'_>> = state.blocks.iter().map(block_line).collect();
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

fn block_line(block: &Block) -> Line<'_> {
    match block {
        Block::User(text) => Line::from(vec![
            Span::styled("❯ ", Style::default().fg(Color::Cyan)),
            Span::raw(text.as_str()),
        ]),
        Block::Assistant(text) => Line::raw(text.as_str()),
        Block::Notice(text) => Line::styled(text.as_str(), Style::default().fg(Color::DarkGray)),
    }
}

fn status(state: &State) -> Paragraph<'_> {
    let body = if !state.hint.is_empty() {
        state.hint.clone()
    } else if state.running {
        "working…  Ctrl-C interrupt".to_string()
    } else {
        "ready  ·  Ctrl-D quit".to_string()
    };
    Paragraph::new(format!(" {body}")).style(Style::default().add_modifier(Modifier::REVERSED))
}

fn input(state: &State) -> Paragraph<'_> {
    // Fully-qualified `ratatui::widgets::Block` here: our transcript `Block` enum
    // shadows the widget name in this module.
    Paragraph::new(format!("❯ {}", state.input))
        .block(ratatui::widgets::Block::default().borders(Borders::TOP))
        .wrap(Wrap { trim: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn append_assistant_coalesces_then_splits_on_end() {
        let mut state = State::new("claude", "opus");
        state.append_assistant("Hel");
        state.append_assistant("lo");
        assert_eq!(state.blocks, vec![Block::Assistant("Hello".to_string())]);
        state.end_stream();
        state.append_assistant("World");
        assert_eq!(
            state.blocks,
            vec![
                Block::Assistant("Hello".to_string()),
                Block::Assistant("World".to_string()),
            ]
        );
    }

    #[test]
    fn append_assistant_ignores_empty_delta() {
        let mut state = State::new("claude", "opus");
        state.append_assistant("");
        assert!(state.blocks.is_empty());
    }

    #[test]
    fn render_writes_expected_text_to_test_backend() {
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut state = State::new("claude", "opus");
        state.tools = 42;
        state.note("connected");
        state.push_user("hello there");
        state.append_assistant("hi human");
        terminal.draw(|frame| render(frame, &state)).expect("draw");

        let buffer = terminal.backend().buffer().clone();
        let text: String = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("Nerve"), "header missing: {text}");
        assert!(text.contains("claude/opus"), "model missing");
        assert!(text.contains("42 tools"), "tool count missing");
        assert!(text.contains("connected"), "notice missing");
        assert!(text.contains("hello there"), "user line missing");
        assert!(text.contains("hi human"), "assistant text missing");
        assert!(text.contains("ready"), "status missing");
    }
}
