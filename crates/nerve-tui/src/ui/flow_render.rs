//! Flow `Block` → wrapped styled lines (C-TUI §2): the orchestration analog of
//! [`super::render`]'s delegate pane.
//!
//! A flow renders as a compact, readable linear transcript: a `⛓` header line,
//! one node pane per node (keyed by `node_id` in [`crate::app::state`], so
//! concurrent nodes don't interleave), a distinct audit row for decisions /
//! budget, and a final outcome row. The node pane mirrors the delegate pane's
//! "header + dim gutter" shape but with its own `▸`/`╎` glyphs so a flow node
//! reads as visually distinct from both the main session and a `/delegate` pane.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use super::palette;
use super::width::{sanitize, wrap_styled};
use crate::app::state::Tone;

/// Render the flow header: `⛓ flow <name> (<strategy>) · <n> nodes`. Cyan `⛓`
/// glyph + bold name, dim strategy/count — distinct from the magenta delegate
/// header.
#[must_use]
pub fn render_flow_header(name: &str, strategy: &str, nodes: usize) -> Vec<Line<'static>> {
    let plural = if nodes == 1 { "" } else { "s" };
    vec![Line::from(vec![
        Span::styled("⛓ flow ".to_string(), palette::cyan()),
        Span::styled(sanitize(name), palette::cyan().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" ({strategy})"), palette::dim()),
        Span::styled(format!(" · {nodes} node{plural}"), palette::dim()),
    ])]
}

/// Render one flow node pane (C-TUI §2): a `▸ <node_id> → <worker>` header
/// (carrying the ✓/✗ + usage once finished) over the node's streamed transcript,
/// each body line behind a cyan `╎` gutter so the node reads as an indented
/// sub-transcript distinct from the `┊` delegate gutter.
#[must_use]
pub fn render_flow_node(
    node_id: &str,
    worker: &str,
    text: &str,
    done: Option<&(bool, String)>,
    cols: usize,
) -> Vec<Line<'static>> {
    let mut header = vec![
        Span::styled("▸ ".to_string(), palette::cyan()),
        Span::styled(
            sanitize(node_id),
            palette::cyan().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" → ".to_string(), palette::dim()),
        Span::styled(sanitize(worker), palette::dim()),
    ];
    if let Some((ok, usage)) = done {
        let (marker, style) = if *ok {
            ("  ✓", palette::green())
        } else {
            ("  ✗", palette::red())
        };
        header.push(Span::styled(marker.to_string(), style));
        if !usage.is_empty() {
            header.push(Span::styled(format!(" {usage}"), palette::dim()));
        }
    }
    let mut lines = vec![Line::from(header)];
    lines.extend(
        wrap_styled(
            &sanitize(text),
            cols.saturating_sub(2).max(1),
            palette::dim(),
        )
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span::styled("╎ ".to_string(), palette::cyan())];
            spans.extend(line.spans);
            Line::from(spans)
        }),
    );
    lines
}

/// Render a flow audit line (a decision / budget note): a toned, single-row entry
/// wrapped to width. Decisions read like `⚖ …`; budget like `◧ …` (the caller
/// supplies the glyph in `text`).
#[must_use]
pub fn render_flow_audit(tone: Tone, text: &str, cols: usize) -> Vec<Line<'static>> {
    let style = match tone {
        Tone::Error => palette::red(),
        Tone::Warn => palette::yellow(),
        Tone::Info => palette::cyan(),
    };
    wrap_styled(&sanitize(text), cols, style)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn flow_header_shows_name_strategy_and_node_count() {
        let lines = render_flow_header("fanout", "parallel", 3);
        let text = plain(&lines);
        assert!(text.contains("⛓ flow fanout"), "{text}");
        assert!(text.contains("(parallel)"), "{text}");
        assert!(text.contains("· 3 nodes"), "{text}");
    }

    #[test]
    fn flow_header_singular_node() {
        assert!(plain(&render_flow_header("s", "single", 1)).contains("· 1 node"));
    }

    #[test]
    fn flow_node_pane_has_header_and_gutter() {
        let lines = render_flow_node("node-0", "claude", "thinking\nhard", None, 40);
        let text = plain(&lines);
        assert!(text.contains("▸ node-0 → claude"), "{text}");
        assert!(text.contains("╎ thinking"), "{text}");
        assert!(text.contains("╎ hard"), "{text}");
    }

    #[test]
    fn flow_node_done_marks_ok_and_usage() {
        let lines = render_flow_node(
            "node-1",
            "codex",
            "out",
            Some(&(true, "↑5 ↓3".to_string())),
            40,
        );
        let text = plain(&lines);
        assert!(text.contains("✓"), "{text}");
        assert!(text.contains("↑5 ↓3"), "{text}");
        // The ✓ marker is green.
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains('✓') && s.style == palette::green())
        );
    }

    #[test]
    fn flow_node_failed_marks_in_red() {
        let lines = render_flow_node("node-2", "grok", "", Some(&(false, String::new())), 40);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content.contains('✗') && s.style == palette::red())
        );
    }

    #[test]
    fn flow_audit_tones_color() {
        let info = render_flow_audit(Tone::Info, "⚖ vote 2/3 ok", 40);
        assert!(info[0].spans.iter().any(|s| s.style == palette::cyan()));
        let warn = render_flow_audit(Tone::Warn, "budget warning", 40);
        assert!(warn[0].spans.iter().any(|s| s.style == palette::yellow()));
    }
}
