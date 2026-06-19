//! Pure UI state + the streaming reduction for the shell.
//!
//! The render path ([`crate::app::render`]) is a pure function of [`State`] →
//! ratatui widgets, so it is testable against a `TestBackend` with no terminal.
//! State carries the editor, slash-command palette, session lifecycle bookkeeping,
//! approval posture, and the token/cost meters (T3); the streaming-coalesce
//! reduction mirrors the TS `app.ts` (`#appendText` / `#finishTool`).

use nerve_runtime::{ApprovalMode, RiskTier};

use crate::ui::editor::Editor;
use crate::ui::models::model_info;
use crate::ui::render::SPINNER;
pub use crate::ui::render::ToolCall;
use crate::ui::theme::{THEMES, theme_index_by_name};

/// Severity tone of a client-side notice (drives its color). Ports the TS notice
/// `tone` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warn,
    Error,
}

/// Lifecycle status of a tool call. Ports the TS tool `status` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

/// Which input the shell is showing: the editor, or the approval modal. In
/// `Approval` the render path draws the modal and key dispatch routes to
/// `on_approval_key` (a decision answers `session.respond`; other keys persist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Input,
    Approval,
}

/// A pending approval request the modal renders and `on_approval_key` answers.
/// Mirrors the TS `state.approval`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalState {
    pub tool: String,
    pub args: String,
    pub request_id: String,
    pub session_id: String,
    pub tier: RiskTier,
    pub preview: String,
}

/// One rendered transcript entry — the full block set. Assistant text is
/// markdown; reasoning is the dim `·`-gutter stream; a tool call carries its
/// framed status cell; a notice carries a tone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A line the user submitted.
    User(String),
    /// Streaming assistant markdown (appended in place as deltas arrive).
    Assistant(String),
    /// Streaming reasoning text (dim, `·` gutter).
    Reasoning(String),
    /// A tool call cell (running → ok/error, with framed output).
    Tool(ToolCall),
    /// A client-side notice (connection status, errors, hints).
    Notice { tone: Tone, text: String },
}

/// The whole shell state. Mutated by the event loop + key handlers; rendered
/// purely by [`crate::app::render`].
#[derive(Debug, Clone)]
pub struct State {
    pub provider: String,
    pub model: String,
    pub tools: usize,
    pub blocks: Vec<Block>,
    /// The multiline input editor (value + cursor + history).
    pub editor: Editor,
    /// Input vs. approval modal.
    pub mode: Mode,
    /// Pending approval, when `mode == Approval` (T4 renders/handles it).
    pub approval: Option<ApprovalState>,
    /// The session's approval posture, shown in the header and pushed on `/mode`.
    pub approval_mode: ApprovalMode,
    /// True while a turn is in flight (drives the status line).
    pub running: bool,
    /// One-shot status hint (e.g. "interrupting…"); cleared on next keypress.
    pub hint: String,
    pub session_id: Option<String>,
    /// Spinner frame, advanced on each tick while running.
    pub spinner: usize,
    /// Expand tool cells (show full capped output instead of a 3-line preview).
    pub expand_tools: bool,
    /// Rows scrolled up from the bottom (0 = pinned to the tail).
    pub scroll: usize,
    /// Selected palette row (slash-command autocomplete).
    pub palette_index: usize,
    /// Accent theme index (cycled by `/theme`).
    pub theme_index: usize,
    /// Wall-clock at the current turn's start (for the elapsed display).
    pub turn_started_at: Option<std::time::Instant>,
    /// Elapsed ms of the current turn (advanced on tick).
    pub elapsed_ms: u64,
    /// Cumulative input/output tokens this session.
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Tokens of the most recent turn's context (drives the context `%`).
    pub last_context_tokens: u64,
    /// Running cost estimate in USD.
    pub cost_usd: f64,
    /// Index of the assistant block currently being streamed into, if any.
    assistant: Option<usize>,
    /// Index of the reasoning block currently being streamed into, if any.
    reasoning: Option<usize>,
}

impl State {
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            tools: 0,
            blocks: Vec::new(),
            editor: Editor::new(),
            mode: Mode::Input,
            approval: None,
            approval_mode: ApprovalMode::Yolo,
            running: false,
            hint: String::new(),
            session_id: None,
            spinner: 0,
            expand_tools: false,
            scroll: 0,
            palette_index: 0,
            theme_index: theme_index_by_name(std::env::var("NERVE_TUI_THEME").ok().as_deref()),
            turn_started_at: None,
            elapsed_ms: 0,
            tokens_in: 0,
            tokens_out: 0,
            last_context_tokens: 0,
            cost_usd: 0.0,
            assistant: None,
            reasoning: None,
        }
    }

    /// The current accent color (theme cycle), used by the header / prompt /
    /// palette selection.
    #[must_use]
    pub fn accent(&self) -> ratatui::style::Color {
        THEMES[self.theme_index % THEMES.len()].color
    }

    /// Push an info-tone notice (connection status, hints).
    pub fn note(&mut self, text: impl Into<String>) {
        self.push_notice(Tone::Info, text);
    }

    /// Push a notice with an explicit tone (warn/error).
    pub fn push_notice(&mut self, tone: Tone, text: impl Into<String>) {
        self.blocks.push(Block::Notice {
            tone,
            text: text.into(),
        });
        self.end_stream();
    }

    /// Push a user message block.
    pub fn push_user(&mut self, text: impl Into<String>) {
        self.blocks.push(Block::User(text.into()));
        self.end_stream();
    }

    /// Append a streaming assistant delta, coalescing into the current assistant
    /// block. Empty deltas are dropped (mirrors TS `#appendText` + the empty-skip
    /// upstream). Starting an assistant run ends any open reasoning run.
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
        self.reasoning = None;
    }

    /// Append a streaming reasoning delta, coalescing into the current reasoning
    /// block. Empty deltas dropped. Starting a reasoning run ends the assistant.
    pub fn append_reasoning(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(index) = self.reasoning
            && let Some(Block::Reasoning(text)) = self.blocks.get_mut(index)
        {
            text.push_str(delta);
            return;
        }
        self.blocks.push(Block::Reasoning(delta.to_string()));
        self.reasoning = Some(self.blocks.len() - 1);
        self.assistant = None;
    }

    /// Start a tool cell (status: running). Ends the open text stream first, as
    /// the TS does on `tool_started`. Stamps the start time for the duration.
    pub fn start_tool(&mut self, tool: impl Into<String>, args: impl Into<String>) {
        self.end_stream();
        self.blocks.push(Block::Tool(ToolCall {
            tool: tool.into(),
            args: args.into(),
            status: ToolStatus::Running,
            output: None,
            duration_ms: None,
            collapsed: true,
            started_at: Some(std::time::Instant::now()),
        }));
    }

    /// Finish the most recent running cell for `tool`, recording its result and
    /// the elapsed duration. Mirrors TS `#finishTool` (search backwards for a
    /// matching running cell). Used in production; tests use `finish_tool_with`.
    pub fn finish_tool(&mut self, tool: &str, ok: bool, output: impl Into<String>) {
        let elapsed = self
            .running_tool(tool)
            .and_then(|cell| cell.started_at)
            .map_or(0, |started| started.elapsed().as_millis() as u64);
        self.finish_tool_with(tool, ok, output, elapsed);
    }

    /// Like [`finish_tool`](Self::finish_tool) but with an explicit duration, so
    /// the reduction is deterministic in tests (no wall-clock).
    pub fn finish_tool_with(
        &mut self,
        tool: &str,
        ok: bool,
        output: impl Into<String>,
        duration_ms: u64,
    ) {
        for block in self.blocks.iter_mut().rev() {
            if let Block::Tool(cell) = block
                && cell.status == ToolStatus::Running
                && cell.tool == tool
            {
                cell.status = if ok {
                    ToolStatus::Ok
                } else {
                    ToolStatus::Error
                };
                cell.output = Some(output.into());
                cell.duration_ms = Some(duration_ms);
                return;
            }
        }
    }

    /// Fold a `usage` agent event into the token/cost meters. Ports the TS
    /// `usage` arm: accumulate in/out tokens, snapshot the context tokens, and
    /// add the per-MTok cost when the model is known.
    pub fn record_usage(&mut self, input_tokens: u64, output_tokens: u64) {
        self.tokens_in += input_tokens;
        self.tokens_out += output_tokens;
        self.last_context_tokens = input_tokens;
        if let Some(info) = model_info(&self.model) {
            self.cost_usd += (input_tokens as f64 / 1e6) * info.input_per_mtok
                + (output_tokens as f64 / 1e6) * info.output_per_mtok;
        }
    }

    /// Reset the per-session meters (used by `/new`).
    pub fn reset_meters(&mut self) {
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.last_context_tokens = 0;
        self.cost_usd = 0.0;
    }

    /// The most recent still-running cell for `tool`, if any.
    fn running_tool(&self, tool: &str) -> Option<&ToolCall> {
        self.blocks.iter().rev().find_map(|b| match b {
            Block::Tool(cell) if cell.status == ToolStatus::Running && cell.tool == tool => {
                Some(cell)
            }
            _ => None,
        })
    }

    /// End both streaming runs so the next delta starts a fresh block.
    pub fn end_stream(&mut self) {
        self.assistant = None;
        self.reasoning = None;
    }

    /// Advance the spinner frame (called on each tick while running).
    pub fn tick_spinner(&mut self) {
        self.spinner = self.spinner.wrapping_add(1) % SPINNER.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn reasoning_and_assistant_streams_are_independent() {
        let mut state = State::new("p", "m");
        state.append_reasoning("th");
        state.append_reasoning("ink");
        state.append_assistant("ans"); // ends reasoning run
        state.append_reasoning("more"); // starts a fresh reasoning block
        assert_eq!(
            state.blocks,
            vec![
                Block::Reasoning("think".into()),
                Block::Assistant("ans".into()),
                Block::Reasoning("more".into()),
            ]
        );
    }

    #[test]
    fn tool_lifecycle_running_then_finished() {
        let mut state = State::new("p", "m");
        state.start_tool("read_file", r#"{"path":"a.rs"}"#);
        assert!(matches!(
            state.blocks.last(),
            Some(Block::Tool(c)) if c.status == ToolStatus::Running
        ));
        state.finish_tool_with("read_file", true, "contents", 12);
        let Some(Block::Tool(cell)) = state.blocks.last() else {
            panic!("expected tool block");
        };
        assert_eq!(cell.status, ToolStatus::Ok);
        assert_eq!(cell.output.as_deref(), Some("contents"));
        assert_eq!(cell.duration_ms, Some(12));
    }

    #[test]
    fn record_usage_accumulates_tokens_and_cost() {
        let mut state = State::new("claude", "claude-opus-4-8");
        state.record_usage(1_000_000, 0);
        // opus input is $15 / MTok.
        assert_eq!(state.tokens_in, 1_000_000);
        assert_eq!(state.last_context_tokens, 1_000_000);
        assert!((state.cost_usd - 15.0).abs() < 1e-9);
        state.record_usage(0, 1_000_000); // +$75 output
        assert!((state.cost_usd - 90.0).abs() < 1e-9);
    }

    #[test]
    fn record_usage_unknown_model_tracks_tokens_only() {
        let mut state = State::new("p", "totally-unknown");
        state.record_usage(500, 200);
        assert_eq!(state.tokens_in, 500);
        assert_eq!(state.cost_usd, 0.0);
    }

    #[test]
    fn reset_meters_clears_token_cost_state() {
        let mut state = State::new("claude", "claude-opus-4-8");
        state.record_usage(1000, 1000);
        state.reset_meters();
        assert_eq!(state.tokens_in, 0);
        assert_eq!(state.tokens_out, 0);
        assert_eq!(state.last_context_tokens, 0);
        assert_eq!(state.cost_usd, 0.0);
    }
}
