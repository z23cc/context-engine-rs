//! Block → wrapped styled lines: the port of `transcript.ts`.
//!
//! Assistant text renders as markdown; reasoning is dimmed with a `·` gutter;
//! user input is cyan + bold with a `❯` marker; tool calls render as a
//! status-colored rounded frame (`╭─ <marker> <tool> · <dur> <args>` / body /
//! `╰──╯`) with collapse/expand of the (capped) output and diff detection; notices
//! are colored by tone. Untrusted text is sanitized.
//!
//! Deviations from the TS, all forced by the ratatui buffer model (no inline
//! OSC-8 escapes in cells):
//! - file paths render as plain text, not OSC-8 hyperlinks (no `linkPath`),
//! - the image-result line shows `🖼  <path>` as text rather than a clickable link.
//!
//! Everything else (frame glyphs, colors, collapse marker, duration format,
//! envelope extraction, diff routing) matches the TS.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::diff::{is_diff, render_diff};
use super::markdown::render_markdown;
use super::palette;
use super::width::{sanitize, truncate_to_width, width, wrap_styled};
use crate::app::state::{Block, Tone, ToolStatus};

const COLLAPSED_OUTPUT_LINES: usize = 3;
const EXPANDED_OUTPUT_LINES: usize = 40;
/// Braille spinner frames, shared with the status bar (was `ansi.ts::SPINNER`).
pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Options controlling block rendering (spinner frame + tool expansion).
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions {
    /// Spinner frame index for the running tool's marker.
    pub spinner: usize,
}

/// Collapse whitespace to a single-line preview. Ports `previewLine`.
#[must_use]
pub fn preview_line(value: &str) -> String {
    sanitize(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Human-readable elapsed duration. Ports `formatDuration`.
#[must_use]
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms as f64 / 1000.0;
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let mins = (seconds / 60.0).floor() as u64;
    let rem = (seconds % 60.0).round() as u64;
    format!("{mins}m {rem}s")
}

/// Extracted tool payload: the human-readable text and whether it is a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolText {
    pub text: String,
    pub diff: bool,
}

/// Pull the human-readable payload out of a tool result. nerve tools return JSON
/// envelopes (`{diff}`, `{content}`, …); surface that text and flag diffs. Ports
/// `extractToolText`.
#[must_use]
pub fn extract_tool_text(output: &str) -> ToolText {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return ToolText {
            text: output.to_string(),
            diff: is_diff(output),
        };
    };
    if let Value::String(s) = &value {
        return ToolText {
            diff: is_diff(s),
            text: s.clone(),
        };
    }
    if let Value::Object(map) = &value {
        if let Some(Value::String(diff)) = map.get("diff") {
            return ToolText {
                text: diff.clone(),
                diff: true,
            };
        }
        if let Some(Value::String(content)) = map.get("content") {
            return ToolText {
                diff: is_diff(content),
                text: content.clone(),
            };
        }
        if let Some(Value::Array(parts)) = map.get("content") {
            let joined = join_text_parts(parts);
            if !joined.is_empty() {
                return ToolText {
                    diff: is_diff(&joined),
                    text: joined,
                };
            }
        }
        for key in ["view", "output", "message", "stdout", "text", "result"] {
            if let Some(Value::String(field)) = map.get(key) {
                return ToolText {
                    diff: is_diff(field),
                    text: field.clone(),
                };
            }
        }
    }
    ToolText {
        text: output.to_string(),
        diff: false,
    }
}

fn join_text_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|p| p.as_object()?.get("text")?.as_str())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render a single block to colored, wrapped lines. Ports `renderBlock`.
#[must_use]
pub fn render_block(block: &Block, cols: usize, opts: RenderOptions) -> Vec<Line<'static>> {
    match block {
        Block::User(text) => render_user(text, cols),
        Block::Assistant(text) => render_markdown(&sanitize(text), cols),
        Block::Reasoning(text) => render_reasoning(text, cols),
        Block::Tool(tool) => render_tool(tool, cols, opts),
        Block::Delegate { agent, text } => render_delegate(agent, text, cols),
        Block::FlowHeader {
            name,
            strategy,
            nodes,
        } => super::flow_render::render_flow_header(name, strategy, *nodes),
        Block::FlowNode {
            node_id,
            worker,
            text,
            done,
        } => super::flow_render::render_flow_node(node_id, worker, text, done.as_ref(), cols),
        Block::FlowAudit { tone, text } => super::flow_render::render_flow_audit(*tone, text, cols),
        Block::Notice { tone, text } => render_notice(*tone, text, cols),
    }
}

/// Flatten all blocks into transcript lines, one blank line between blocks.
/// Ports `blocksToLines`.
#[must_use]
pub fn blocks_to_lines(blocks: &[Block], cols: usize, opts: RenderOptions) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (idx, block) in blocks.iter().enumerate() {
        if idx > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(render_block(block, cols, opts));
    }
    lines
}

fn render_user(text: &str, cols: usize) -> Vec<Line<'static>> {
    let style = palette::cyan().add_modifier(ratatui::style::Modifier::BOLD);
    let wrapped = wrap_styled(&sanitize(text), cols.saturating_sub(2).max(1), style);
    wrapped
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            let marker = if idx == 0 { "❯ " } else { "  " };
            let mut spans = vec![Span::styled(marker.to_string(), palette::cyan())];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

fn render_reasoning(text: &str, cols: usize) -> Vec<Line<'static>> {
    wrap_styled(
        &sanitize(text),
        cols.saturating_sub(2).max(1),
        palette::dim(),
    )
    .into_iter()
    .map(|line| {
        let mut spans = vec![Span::styled("· ".to_string(), palette::dim())];
        spans.extend(line.spans);
        Line::from(spans)
    })
    .collect()
}

/// A delegate block: a magenta `⟳ delegating → <agent>` header over the streamed
/// agent output (dim, `┊`-gutter), so a delegated run reads as a distinct,
/// indented sub-transcript rather than the parent's own assistant text.
fn render_delegate(agent: &str, text: &str, cols: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled("⟳ delegating → ".to_string(), palette::magenta()),
        Span::styled(
            agent.to_string(),
            palette::magenta().add_modifier(ratatui::style::Modifier::BOLD),
        ),
    ])];
    lines.extend(
        wrap_styled(
            &sanitize(text),
            cols.saturating_sub(2).max(1),
            palette::dim(),
        )
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("┊ ".to_string(), palette::magenta())];
            spans.extend(line.spans);
            Line::from(spans)
        }),
    );
    lines
}

fn render_notice(tone: Tone, text: &str, cols: usize) -> Vec<Line<'static>> {
    let style = match tone {
        Tone::Error => palette::red(),
        Tone::Warn => palette::yellow(),
        Tone::Info => palette::dim(),
    };
    wrap_styled(&sanitize(text), cols, style)
}

/// A tool block carries the running/finished call. Renders header-only while
/// running or empty, otherwise a framed body.
fn render_tool(tool: &ToolCall, cols: usize, opts: RenderOptions) -> Vec<Line<'static>> {
    let header = tool_header(tool, opts.spinner);
    let color = tool_color(tool.status);
    let Some(output) = tool.output.as_deref() else {
        return vec![truncate_line(header, cols)];
    };
    if tool.status == ToolStatus::Running {
        return vec![truncate_line(header, cols)];
    }
    if let Some(path) = image_path(output) {
        let body = vec![Line::from(Span::styled(
            format!("🖼  {path}"),
            palette::dim(),
        ))];
        return frame(header, body, cols, color);
    }
    let extracted = extract_tool_text(output);
    if extracted.text.trim().is_empty() {
        return vec![truncate_line(header, cols)];
    }
    let body = tool_body(&extracted, tool.status, cols, tool.collapsed);
    frame(header, body, cols, color)
}

/// Build the (capped, collapse-aware) body lines of a tool frame.
fn tool_body(
    extracted: &ToolText,
    status: ToolStatus,
    cols: usize,
    collapsed: bool,
) -> Vec<Line<'static>> {
    let inner = cols.saturating_sub(4).max(1);
    let all_lines: Vec<Line<'static>> = if extracted.diff {
        render_diff(&extracted.text)
    } else {
        let out_color = if status == ToolStatus::Error {
            palette::red()
        } else {
            palette::dim()
        };
        sanitize(&extracted.text)
            .split('\n')
            .flat_map(|line| wrap_styled(line, inner, out_color))
            .collect()
    };
    let limit = if collapsed {
        COLLAPSED_OUTPUT_LINES
    } else {
        EXPANDED_OUTPUT_LINES
    };
    let mut body: Vec<Line<'static>> = all_lines.iter().take(limit).cloned().collect();
    let hidden = all_lines.len().saturating_sub(body.len());
    if hidden > 0 {
        let plural = if hidden > 1 { "s" } else { "" };
        body.push(Line::from(Span::styled(
            format!("… +{hidden} more line{plural} (Ctrl-O)"),
            palette::dim(),
        )));
    }
    body
}

fn tool_color(status: ToolStatus) -> Style {
    match status {
        ToolStatus::Running => palette::yellow(),
        ToolStatus::Ok => palette::green(),
        ToolStatus::Error => palette::red(),
    }
}

/// Build the tool header line: `<marker> <tool> · <dur> <args>`. Ports
/// `toolHeader`; duration precedes args so it survives truncation.
fn tool_header(tool: &ToolCall, spinner: usize) -> Line<'static> {
    let (marker, marker_style) = match tool.status {
        ToolStatus::Running => (
            SPINNER[spinner % SPINNER.len()].to_string(),
            palette::yellow(),
        ),
        ToolStatus::Ok => ("✓".to_string(), palette::green()),
        ToolStatus::Error => ("✗".to_string(), palette::red()),
    };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(tool.tool.clone(), palette::cyan()),
    ];
    if let Some(ms) = tool.duration_ms {
        spans.push(Span::styled(
            format!(" · {}", format_duration(ms)),
            palette::dim(),
        ));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled(tool_args_display(&tool.args), palette::dim()));
    Line::from(spans)
}

/// Prefer a clean `path` over raw JSON args (no OSC-8 link in the buffer model).
/// Ports `toolArgsDisplay` minus the hyperlink wrap.
fn tool_args_display(args: &str) -> String {
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(args)
        && let Some(Value::String(path)) = map.get("path")
    {
        return path.clone();
    }
    preview_line(args)
}

/// Match the first absolute image path in a tool result (image tools save files).
fn image_path(output: &str) -> Option<String> {
    // Find a `/…/<name>.<ext>` token where ext is an image extension.
    let exts = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
    for token in output.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        if !token.starts_with('/') {
            continue;
        }
        let lower = token.to_ascii_lowercase();
        if exts.iter().any(|e| lower.ends_with(&format!(".{e}"))) {
            return Some(token.to_string());
        }
    }
    None
}

/// Wrap `header` + `body` in a status-colored rounded box `cols` wide. Ports
/// `frame`: `╭─ <head> ─╮` / `│ <line> │` / `╰──╯`.
fn frame(
    header: Line<'static>,
    body: Vec<Line<'static>>,
    cols: usize,
    color: Style,
) -> Vec<Line<'static>> {
    if cols < 6 {
        let mut out = vec![header];
        out.extend(body);
        return out;
    }
    let head = truncate_runs_to_width(&header, cols.saturating_sub(6));
    let head_width = line_width(&head);
    let dashes = cols.saturating_sub(5 + head_width);
    let inner = cols.saturating_sub(4).max(1);
    let mut lines = Vec::with_capacity(body.len() + 2);

    // Top: `╭─ ` + head + ` ───╮`
    let mut top = vec![Span::styled("╭─ ".to_string(), color)];
    top.extend(head.spans);
    top.push(Span::styled(format!(" {}╮", "─".repeat(dashes)), color));
    lines.push(Line::from(top));

    for line in body {
        let padded = pad_line_to_width(truncate_runs_to_width(&line, inner), inner);
        let mut spans = vec![Span::styled("│ ".to_string(), color)];
        spans.extend(padded.spans);
        spans.push(Span::styled(" │".to_string(), color));
        lines.push(Line::from(spans));
    }

    // Bottom: `╰────╯`
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(cols.saturating_sub(2))),
        color,
    )));
    lines
}

/// Total display width of a line's spans.
fn line_width(line: &Line<'static>) -> usize {
    line.spans.iter().map(|s| width(s.content.as_ref())).sum()
}

/// Truncate a single (already-built) styled line to `cols` columns, appending a
/// dim `…` when content is dropped.
fn truncate_line(line: Line<'static>, cols: usize) -> Line<'static> {
    truncate_runs_to_width(&line, cols)
}

/// Truncate the spans of a line to `cols` columns, preserving per-span style.
fn truncate_runs_to_width(line: &Line<'static>, cols: usize) -> Line<'static> {
    if cols == 0 {
        return Line::from("");
    }
    if line_width(line) <= cols {
        return line.clone();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in &line.spans {
        let remaining = cols.saturating_sub(1).saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let kept = truncate_to_width(span.content.as_ref(), remaining + 1);
        let ellipsized = kept.ends_with('…') && !span.content.ends_with('…');
        let text = if ellipsized {
            kept.trim_end_matches('…').to_string()
        } else {
            kept
        };
        used += width(&text);
        spans.push(Span::styled(text, span.style));
        if used >= cols.saturating_sub(1) {
            break;
        }
    }
    spans.push(Span::styled("…".to_string(), palette::dim()));
    Line::from(spans)
}

/// Right-pad a line with spaces to exactly `cols` columns (no truncation here).
fn pad_line_to_width(line: Line<'static>, cols: usize) -> Line<'static> {
    let w = line_width(&line);
    let mut spans = line.spans;
    if w < cols {
        spans.push(Span::raw(" ".repeat(cols - w)));
    }
    Line::from(spans)
}

/// A tool call block. Defined here (consumed by [`render_tool`]); re-exported via
/// `app::state` as part of the [`Block`] enum.
///
/// `started_at` is the host wall-clock at `tool_started`, used to compute the
/// frame's duration (the TS keeps the same `startedAt` per block). It is excluded
/// from equality so blocks stay comparable in tests/snapshots regardless of when
/// they ran.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool: String,
    /// Raw JSON arguments string (preview-formatted at render time).
    pub args: String,
    pub status: ToolStatus,
    /// Output payload once the tool finishes (None while running).
    pub output: Option<String>,
    /// Wall-clock ms the tool ran (None until finished).
    pub duration_ms: Option<u64>,
    /// Collapse the output to a 3-line preview (toggled by Ctrl-O).
    pub collapsed: bool,
    /// Host clock at tool start; excluded from equality (see type docs).
    pub started_at: Option<std::time::Instant>,
}

impl PartialEq for ToolCall {
    fn eq(&self, other: &Self) -> bool {
        self.tool == other.tool
            && self.args == other.args
            && self.status == other.status
            && self.output == other.output
            && self.duration_ms == other.duration_ms
            && self.collapsed == other.collapsed
    }
}

impl Eq for ToolCall {}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn plain(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn tool(status: ToolStatus, output: Option<&str>, collapsed: bool) -> Block {
        Block::Tool(ToolCall {
            tool: "read_file".into(),
            args: r#"{"path":"a.rs"}"#.into(),
            status,
            output: output.map(str::to_string),
            duration_ms: Some(320),
            collapsed,
            started_at: None,
        })
    }

    #[test]
    fn format_duration_is_human_readable() {
        assert_eq!(format_duration(320), "320ms");
        assert_eq!(format_duration(1500), "1.5s");
        assert_eq!(format_duration(65000), "1m 5s");
    }

    #[test]
    fn preview_line_collapses_whitespace() {
        assert_eq!(preview_line("a\n  b\tc"), "a b c");
    }

    #[test]
    fn user_block_has_marker_and_accent() {
        let lines = render_block(
            &Block::User("hello there".into()),
            40,
            RenderOptions::default(),
        );
        assert!(plain(&lines).contains("❯ hello there"));
    }

    #[test]
    fn running_tool_is_header_only() {
        let lines = render_block(
            &Block::Tool(ToolCall {
                tool: "edit".into(),
                args: String::new(),
                status: ToolStatus::Running,
                output: None,
                duration_ms: None,
                collapsed: true,
                started_at: None,
            }),
            40,
            RenderOptions { spinner: 0 },
        );
        assert_eq!(lines.len(), 1);
        let text = plain(&lines);
        assert!(text.contains("edit"));
        assert!(text.contains(SPINNER[0]));
    }

    #[test]
    fn finished_tool_frames_with_duration() {
        let lines = render_block(
            &tool(ToolStatus::Ok, Some("a\nb\nc\nd"), true),
            40,
            RenderOptions::default(),
        );
        let joined = plain(&lines);
        assert!(joined.contains("╭─ ✓ read_file"), "{joined}");
        assert!(joined.contains("· 320ms"), "{joined}");
        assert!(lines.iter().any(|l| {
            let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            t.starts_with('╰') && t.ends_with('╯')
        }));
    }

    #[test]
    fn failed_tool_shows_error_output_in_red() {
        let lines = render_block(
            &Block::Tool(ToolCall {
                tool: "edit".into(),
                args: String::new(),
                status: ToolStatus::Error,
                output: Some("boom".into()),
                duration_ms: None,
                collapsed: true,
                started_at: None,
            }),
            40,
            RenderOptions::default(),
        );
        let joined = plain(&lines);
        assert!(joined.contains("✗ edit"), "{joined}");
        assert!(joined.contains("boom"), "{joined}");
        // The body text is red on error.
        assert!(
            lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|s| s.content.contains("boom") && s.style == palette::red())
        );
    }

    #[test]
    fn collapse_caps_output_expand_shows_all() {
        let out = "l1\nl2\nl3\nl4\nl5\nl6";
        let collapsed = render_block(
            &tool(ToolStatus::Ok, Some(out), true),
            40,
            RenderOptions::default(),
        );
        assert!(plain(&collapsed).contains("more line"));
        let expanded = render_block(
            &tool(ToolStatus::Ok, Some(out), false),
            40,
            RenderOptions::default(),
        );
        let joined = plain(&expanded);
        assert!(joined.contains("l6"), "{joined}");
        assert!(!joined.contains("more line"), "{joined}");
    }

    #[test]
    fn extract_tool_text_surfaces_envelopes() {
        assert_eq!(
            extract_tool_text(r#"{"diff":"@@ -1 +1 @@\n-a\n+b"}"#),
            ToolText {
                text: "@@ -1 +1 @@\n-a\n+b".into(),
                diff: true
            }
        );
        assert_eq!(
            extract_tool_text(r#"{"content":"hello"}"#),
            ToolText {
                text: "hello".into(),
                diff: false
            }
        );
        assert_eq!(
            extract_tool_text("plain text"),
            ToolText {
                text: "plain text".into(),
                diff: false
            }
        );
    }

    #[test]
    fn tool_cell_renders_colored_diff() {
        let output = serde_json::json!({ "diff": "@@ -1 +1 @@\n-foo\n+bar" }).to_string();
        let lines = render_block(
            &Block::Tool(ToolCall {
                tool: "edit".into(),
                args: r#"{"path":"a.rs"}"#.into(),
                status: ToolStatus::Ok,
                output: Some(output),
                duration_ms: None,
                collapsed: false,
                started_at: None,
            }),
            50,
            RenderOptions::default(),
        );
        let joined = plain(&lines);
        assert!(joined.contains("╭─ ✓ edit"), "{joined}");
        assert!(joined.contains("-foo"), "{joined}");
        assert!(joined.contains("+bar"), "{joined}");
    }

    #[test]
    fn image_result_renders_thumbnail_line() {
        let lines = render_block(
            &Block::Tool(ToolCall {
                tool: "openai_image_generate".into(),
                args: "{}".into(),
                status: ToolStatus::Ok,
                output: Some(r#"{"path":"/tmp/out.png"}"#.into()),
                duration_ms: None,
                collapsed: true,
                started_at: None,
            }),
            50,
            RenderOptions::default(),
        );
        let joined = plain(&lines);
        assert!(joined.contains('🖼'), "{joined}");
        assert!(joined.contains("out.png"), "{joined}");
    }

    #[test]
    fn reasoning_has_dim_gutter() {
        let lines = render_block(
            &Block::Reasoning("thinking".into()),
            40,
            RenderOptions::default(),
        );
        assert!(plain(&lines).contains("· thinking"));
        assert!(lines[0].spans[0].style == palette::dim());
    }

    #[test]
    fn delegate_block_has_header_and_gutter() {
        let lines = render_block(
            &Block::Delegate {
                agent: "codex".into(),
                text: "applying patch\nrunning tests".into(),
            },
            40,
            RenderOptions::default(),
        );
        let joined = plain(&lines);
        assert!(joined.contains("⟳ delegating → codex"), "{joined}");
        assert!(joined.contains("┊ applying patch"), "{joined}");
        assert!(joined.contains("┊ running tests"), "{joined}");
        // Header is magenta; the agent name is also bold.
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains("delegating") && s.style == palette::magenta())
        );
        assert!(lines[0].spans.iter().any(|s| {
            s.content.contains("codex")
                && s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)
        }));
    }

    #[test]
    fn notice_tones_color() {
        let err = render_block(
            &Block::Notice {
                tone: Tone::Error,
                text: "bad".into(),
            },
            40,
            RenderOptions::default(),
        );
        assert!(err[0].spans.iter().any(|s| s.style == palette::red()));
        let warn = render_block(
            &Block::Notice {
                tone: Tone::Warn,
                text: "hmm".into(),
            },
            40,
            RenderOptions::default(),
        );
        assert!(warn[0].spans.iter().any(|s| s.style == palette::yellow()));
    }

    #[test]
    fn blocks_to_lines_separates_with_blank() {
        let blocks = vec![Block::User("hi".into()), Block::Assistant("yo".into())];
        let lines = blocks_to_lines(&blocks, 40, RenderOptions::default());
        assert!(
            lines
                .iter()
                .any(|l| l.spans.is_empty() || plain(std::slice::from_ref(l)).is_empty())
        );
        let joined = plain(&lines);
        assert!(joined.contains("hi") && joined.contains("yo"));
    }

    #[test]
    fn frame_renders_into_test_backend() {
        let block = tool(ToolStatus::Ok, Some("hello\nworld"), false);
        let lines = render_block(&block, 30, RenderOptions::default());
        let backend = TestBackend::new(30, lines.len() as u16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(ratatui::widgets::Paragraph::new(lines.clone()), f.area());
            })
            .unwrap();
        let buf: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(buf.contains("read_file"));
        assert!(buf.contains('╭') && buf.contains('╯'));
    }

    /// A full flow transcript (header → two node panes → audit → outcome) renders
    /// the orchestration shape; the snapshot pins the glyphs/colors (C-TUI §2).
    #[test]
    fn snapshot_flow_transcript() {
        let blocks = vec![
            Block::FlowHeader {
                name: "parallel".into(),
                strategy: "parallel".into(),
                nodes: 2,
            },
            Block::FlowNode {
                node_id: "node-0".into(),
                worker: "claude".into(),
                text: "alpha answer".into(),
                done: Some((true, "↑5 ↓3".into())),
            },
            Block::FlowNode {
                node_id: "node-1".into(),
                worker: "codex".into(),
                text: "beta answer".into(),
                done: Some((true, String::new())),
            },
            Block::FlowAudit {
                tone: Tone::Info,
                text: "⚖ judge picked → node-0".into(),
            },
            Block::FlowAudit {
                tone: Tone::Info,
                text: "✓ flow done · parallel: 2/2 ok".into(),
            },
        ];
        let lines = blocks_to_lines(&blocks, 60, RenderOptions::default());
        let rendered = lines.iter().map(styled_line).collect::<Vec<_>>().join("\n");
        insta::assert_snapshot!(rendered);
    }

    /// Serialize a styled line as `«tag»text` segments (mirrors `app::render`'s
    /// snapshot helper) so the snapshot pins glyphs + colors.
    fn styled_line(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|s| {
                let mut parts = Vec::new();
                if let Some(fg) = s.style.fg {
                    parts.push(format!("{fg:?}").to_lowercase());
                }
                if s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)
                {
                    parts.push("bold".into());
                }
                if s.style.add_modifier.contains(ratatui::style::Modifier::DIM) {
                    parts.push("dim".into());
                }
                let tag = parts.join("+");
                if tag.is_empty() {
                    s.content.to_string()
                } else {
                    format!("«{tag}»{}", s.content)
                }
            })
            .collect()
    }
}
