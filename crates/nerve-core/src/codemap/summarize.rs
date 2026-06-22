use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use tree_sitter::Node;

use super::language::Language;

const DEFAULT_MIN_BODY_LINES: usize = 4;
const DEFAULT_MIN_COMMENT_LINES: usize = 6;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct SummaryOptions {
    pub(super) min_body_lines: usize,
    pub(super) min_comment_lines: usize,
    pub(super) unfold_until_lines: usize,
    pub(super) unfold_limit_lines: usize,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            min_body_lines: DEFAULT_MIN_BODY_LINES,
            min_comment_lines: DEFAULT_MIN_COMMENT_LINES,
            unfold_until_lines: 0,
            unfold_limit_lines: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarySegment {
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryResult {
    pub language: Option<String>,
    pub parsed: bool,
    pub elided: bool,
    pub total_lines: usize,
    pub segments: Vec<SummarySegment>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineSpan {
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct SpanNode {
    span: LineSpan,
    children: Vec<usize>,
}

#[derive(Debug, Default)]
struct ElidableForest {
    nodes: Vec<SpanNode>,
    roots: Vec<usize>,
}

impl ElidableForest {
    fn push(&mut self, parent: Option<usize>, span: LineSpan) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(SpanNode {
            span,
            children: Vec::new(),
        });
        match parent {
            Some(parent) => self.nodes[parent].children.push(idx),
            None => self.roots.push(idx),
        }
        idx
    }
}

pub fn summarize_source(path: &str, source: &str) -> SummaryResult {
    summarize_source_with_options(path, source, SummaryOptions::default())
}

fn summarize_source_with_options(
    path: &str,
    source: &str,
    options: SummaryOptions,
) -> SummaryResult {
    super::summary_cache::get_or_insert_with(path, source, options, || {
        summarize_source_uncached(path, source, options)
    })
}

fn summarize_source_uncached(path: &str, source: &str, options: SummaryOptions) -> SummaryResult {
    let total_lines = count_lines(source);
    if super::markdown::is_markdown_path(path) {
        return summarize_markdown_source(source, total_lines, options);
    }
    let Some(language) = Language::from_path_or_source(path, source) else {
        return full_content_result(source, total_lines);
    };
    summarize_parsed_source(language, source, total_lines, options)
}

fn summarize_markdown_source(
    source: &str,
    total_lines: usize,
    options: SummaryOptions,
) -> SummaryResult {
    let lines = split_line_segments(source);
    let fences = super::markdown::supported_fences(&lines);
    if fences.is_empty() {
        return full_content_result(source, total_lines);
    }

    let mut parsed_any = false;
    let mut spans = Vec::new();
    for fence in fences {
        let body_source = super::markdown::fence_source(&lines, &fence);
        let body_lines = count_lines(&body_source);
        let body_summary =
            summarize_parsed_source(fence.language, &body_source, body_lines, options);
        parsed_any |= body_summary.parsed;
        for segment in body_summary
            .segments
            .into_iter()
            .filter(|segment| segment.kind == "elided")
        {
            spans.push(LineSpan {
                start: fence.body.start + segment.start_line,
                end: fence.body.start + segment.end_line,
            });
        }
    }
    if !parsed_any {
        return full_content_result(source, total_lines);
    }

    let spans = normalize_spans(spans, total_lines);
    SummaryResult {
        language: Some("markdown".to_string()),
        parsed: true,
        elided: !spans.is_empty(),
        total_lines,
        segments: build_segments(source, total_lines, &spans),
    }
}

fn summarize_parsed_source(
    language: Language,
    source: &str,
    total_lines: usize,
    options: SummaryOptions,
) -> SummaryResult {
    let Some(tree) = parse_tree(language, source) else {
        return full_content_result(source, total_lines);
    };
    let root = tree.root_node();
    if root.has_error() {
        return full_content_result(source, total_lines);
    }

    let mut forest = ElidableForest::default();
    collect_elidable_tree(root, None, language, &options, &mut forest);
    let spans = select_folded_spans(
        &forest,
        total_lines,
        options.unfold_until_lines,
        options.unfold_limit_lines,
    );
    let spans = normalize_spans(spans, total_lines);
    let segments = build_segments(source, total_lines, &spans);
    SummaryResult {
        language: Some(language.name().to_string()),
        parsed: true,
        elided: !spans.is_empty(),
        total_lines,
        segments,
    }
}

pub fn render_summary(path: &str, result: &SummaryResult) -> String {
    let mut out = String::new();
    let ranges: Vec<String> = result
        .segments
        .iter()
        .filter(|segment| segment.kind == "elided")
        .map(|segment| format!("{}-{}", segment.start_line, segment.end_line))
        .collect();

    for segment in &result.segments {
        if segment.kind == "kept" {
            if let Some(text) = &segment.text {
                out.push_str(text);
            }
        } else {
            out.push_str(&format!(
                "… elided lines {}-{} …\n",
                segment.start_line, segment.end_line
            ));
        }
    }
    if !ranges.is_empty() {
        out.push_str("\n---\n");
        out.push_str(&format!("Elided ranges: {}:{}\n", path, ranges.join(",")));
        out.push_str("Re-read with read_file start_line/end_line for any range above.\n");
    }
    out
}

fn parse_tree(language: Language, source: &str) -> Option<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.ts_language()).ok()?;
    parser.parse(source, None)
}

fn full_content_result(source: &str, total_lines: usize) -> SummaryResult {
    let segments = if source.is_empty() {
        Vec::new()
    } else {
        vec![SummarySegment {
            kind: "kept".to_string(),
            start_line: 1,
            end_line: total_lines,
            text: Some(source.to_string()),
        }]
    };
    SummaryResult {
        language: None,
        parsed: false,
        elided: false,
        total_lines,
        segments,
    }
}

fn count_lines(source: &str) -> usize {
    if source.is_empty() {
        0
    } else {
        source.lines().count().max(1)
    }
}

fn collect_elidable_tree(
    node: Node<'_>,
    parent: Option<usize>,
    language: Language,
    options: &SummaryOptions,
    forest: &mut ElidableForest,
) {
    let lines = node_line_count(node);
    if is_comment_kind(language, node.kind()) {
        push_comment_span(node, lines, parent, options, forest);
        return;
    }

    let mut current_parent = parent;
    if is_elidable_kind(language, node.kind()) && lines >= options.min_body_lines {
        let start = node_start_line(node).saturating_add(1);
        let end = node_content_end_line(node).saturating_sub(1);
        if start <= end {
            current_parent = Some(forest.push(parent, LineSpan { start, end }));
        }
    }

    collect_groupable_runs(node, language, current_parent, options, forest);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_elidable_tree(child, current_parent, language, options, forest);
    }
}

fn push_comment_span(
    node: Node<'_>,
    lines: usize,
    parent: Option<usize>,
    options: &SummaryOptions,
    forest: &mut ElidableForest,
) {
    if lines < options.min_comment_lines {
        return;
    }
    let start = node_start_line(node).saturating_add(1);
    let end = node_content_end_line(node).saturating_sub(1);
    if start <= end {
        forest.push(parent, LineSpan { start, end });
    }
}

fn collect_groupable_runs(
    node: Node<'_>,
    language: Language,
    parent: Option<usize>,
    options: &SummaryOptions,
    forest: &mut ElidableForest,
) {
    let mut first: Option<Node<'_>> = None;
    let mut last: Option<Node<'_>> = None;
    let mut count = 0usize;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if is_groupable_kind(language, child.kind()) {
            first.get_or_insert(child);
            last = Some(child);
            count += 1;
        } else {
            flush_groupable_run(first, last, count, parent, options, forest);
            first = None;
            last = None;
            count = 0;
        }
    }
    flush_groupable_run(first, last, count, parent, options, forest);
}

fn flush_groupable_run(
    first: Option<Node<'_>>,
    last: Option<Node<'_>>,
    count: usize,
    parent: Option<usize>,
    options: &SummaryOptions,
    forest: &mut ElidableForest,
) {
    let (Some(first), Some(last)) = (first, last) else {
        return;
    };
    if count < 2 {
        return;
    }
    let span_lines = node_content_end_line(last).saturating_sub(node_start_line(first)) + 1;
    if span_lines < options.min_body_lines {
        return;
    }
    let start = node_content_end_line(first).saturating_add(1);
    let end = node_start_line(last).saturating_sub(1);
    if start <= end {
        forest.push(parent, LineSpan { start, end });
    }
}

fn select_folded_spans(
    forest: &ElidableForest,
    total_lines: usize,
    unfold_until: usize,
    unfold_limit: usize,
) -> Vec<LineSpan> {
    let mut folded: BTreeSet<usize> = forest.roots.iter().copied().collect();
    if unfold_until == 0 || folded.is_empty() {
        return folded
            .into_iter()
            .map(|idx| forest.nodes[idx].span)
            .collect();
    }
    let mut visible = visible_lines(forest, &folded, total_lines);
    let mut queue: VecDeque<usize> = forest.roots.iter().copied().collect();
    while let Some(idx) = queue.pop_front() {
        if visible >= unfold_until || !folded.contains(&idx) {
            continue;
        }
        let node = &forest.nodes[idx];
        let child_lines = node
            .children
            .iter()
            .map(|&child| forest.nodes[child].span.lines())
            .sum();
        let new_visible = visible.saturating_add(node.span.lines().saturating_sub(child_lines));
        if new_visible > unfold_limit {
            break;
        }
        folded.remove(&idx);
        for &child in &node.children {
            folded.insert(child);
            queue.push_back(child);
        }
        visible = new_visible;
    }
    folded
        .into_iter()
        .map(|idx| forest.nodes[idx].span)
        .collect()
}

fn visible_lines(forest: &ElidableForest, folded: &BTreeSet<usize>, total_lines: usize) -> usize {
    let hidden = folded
        .iter()
        .map(|&idx| forest.nodes[idx].span.lines())
        .sum::<usize>();
    total_lines.saturating_sub(hidden)
}

impl LineSpan {
    fn lines(self) -> usize {
        self.end.saturating_sub(self.start).saturating_add(1)
    }
}

fn normalize_spans(mut spans: Vec<LineSpan>, total_lines: usize) -> Vec<LineSpan> {
    spans.retain(|span| span.start <= span.end && span.start <= total_lines);
    for span in &mut spans {
        span.end = span.end.min(total_lines);
    }
    spans.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<LineSpan> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut()
            && span.start <= last.end.saturating_add(1)
        {
            last.end = last.end.max(span.end);
            continue;
        }
        merged.push(span);
    }
    merged
}

fn build_segments(source: &str, total_lines: usize, spans: &[LineSpan]) -> Vec<SummarySegment> {
    if total_lines == 0 {
        return Vec::new();
    }
    let lines = split_line_segments(source);
    let elided = spans
        .iter()
        .flat_map(|span| span.start..=span.end)
        .collect::<BTreeSet<_>>();
    let mut segments = Vec::new();
    let mut current: Option<(&str, usize, String)> = None;
    for line_number in 1..=total_lines {
        let kind = if elided.contains(&line_number) {
            "elided"
        } else {
            "kept"
        };
        push_line_segment(&mut segments, &mut current, kind, line_number, &lines);
    }
    if let Some((kind, start_line, text)) = current {
        push_finished_segment(&mut segments, kind, start_line, total_lines, text);
    }
    segments
}

fn push_line_segment(
    segments: &mut Vec<SummarySegment>,
    current: &mut Option<(&'static str, usize, String)>,
    kind: &'static str,
    line_number: usize,
    lines: &[&str],
) {
    if current
        .as_ref()
        .is_some_and(|(existing, _, _)| *existing != kind)
    {
        let (finished_kind, start_line, text) = current.take().expect("segment present");
        push_finished_segment(segments, finished_kind, start_line, line_number - 1, text);
    }
    let entry = current.get_or_insert_with(|| (kind, line_number, String::new()));
    if kind == "kept" {
        entry
            .2
            .push_str(lines.get(line_number - 1).copied().unwrap_or_default());
    }
}

fn push_finished_segment(
    segments: &mut Vec<SummarySegment>,
    kind: &str,
    start_line: usize,
    end_line: usize,
    text: String,
) {
    segments.push(SummarySegment {
        kind: kind.to_string(),
        start_line,
        end_line,
        text: (kind == "kept").then_some(text),
    });
}

fn split_line_segments(source: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = source.split_inclusive('\n').collect();
    if lines.is_empty() && !source.is_empty() {
        lines.push(source);
    }
    lines
}

fn node_start_line(node: Node<'_>) -> usize {
    node.start_position().row + 1
}

fn node_content_end_line(node: Node<'_>) -> usize {
    let pos = node.end_position();
    let row = if pos.column == 0 && pos.row > 0 {
        pos.row - 1
    } else {
        pos.row
    };
    row + 1
}

fn node_line_count(node: Node<'_>) -> usize {
    node_content_end_line(node)
        .saturating_sub(node_start_line(node))
        .saturating_add(1)
}

fn is_comment_kind(language: Language, kind: &str) -> bool {
    match language {
        Language::Rust => kind == "block_comment",
        Language::Java => kind == "block_comment",
        Language::Python
        | Language::JavaScript
        | Language::TypeScript
        | Language::Tsx
        | Language::Go
        | Language::C
        | Language::Cpp
        | Language::CSharp
        | Language::Ruby
        | Language::Php => kind == "comment",
    }
}

fn is_elidable_kind(language: Language, kind: &str) -> bool {
    match language {
        Language::Rust => matches!(
            kind,
            "block"
                | "match_block"
                | "declaration_list"
                | "field_declaration_list"
                | "ordered_field_declaration_list"
                | "enum_variant_list"
                | "token_tree"
                | "macro_definition"
                | "use_list"
        ),
        Language::Python => matches!(
            kind,
            "block" | "dictionary" | "list" | "set" | "tuple" | "string" | "argument_list"
        ),
        Language::JavaScript | Language::TypeScript | Language::Tsx => matches!(
            kind,
            "statement_block"
                | "function_body"
                | "class_body"
                | "interface_body"
                | "enum_body"
                | "object"
                | "array"
                | "switch_body"
                | "object_type"
        ),
        Language::Go => matches!(
            kind,
            "block" | "composite_literal" | "import_spec_list" | "field_declaration_list"
        ),
        Language::Java => matches!(
            kind,
            "block" | "class_body" | "interface_body" | "enum_body" | "constructor_body"
        ),
        Language::C | Language::Cpp => matches!(
            kind,
            "compound_statement"
                | "field_declaration_list"
                | "enumerator_list"
                | "initializer_list"
        ),
        Language::CSharp => matches!(
            kind,
            "block" | "declaration_list" | "enum_member_declaration_list"
        ),
        Language::Ruby => matches!(kind, "body_statement" | "method" | "do_block" | "block"),
        Language::Php => matches!(
            kind,
            "compound_statement" | "declaration_list" | "match_block"
        ),
    }
}

fn is_groupable_kind(language: Language, kind: &str) -> bool {
    match language {
        Language::Rust => matches!(kind, "use_declaration" | "extern_crate_declaration"),
        Language::Python => matches!(
            kind,
            "import_statement" | "import_from_statement" | "future_import_statement"
        ),
        Language::JavaScript | Language::TypeScript | Language::Tsx => kind == "import_statement",
        Language::Go => kind == "import_declaration",
        Language::Java => kind == "import_declaration",
        Language::C | Language::Cpp => kind == "preproc_include",
        Language::CSharp => kind == "using_directive",
        Language::Php => kind == "namespace_use_declaration",
        Language::Ruby => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_rust_function_body() {
        let source = "pub fn greet() {\n    let name = \"world\";\n    let label = name.to_uppercase();\n    println!(\"{label}\");\n}\n";
        let summary = summarize_source("sample.rs", source);
        assert!(summary.parsed);
        assert!(summary.elided);
        assert_eq!(
            summary.segments[0].text.as_deref(),
            Some("pub fn greet() {\n")
        );
        assert_eq!(summary.segments[1].start_line, 2);
        assert_eq!(summary.segments[1].end_line, 4);
        assert_eq!(summary.segments[2].text.as_deref(), Some("}\n"));
    }

    #[test]
    fn summarizes_import_run() {
        let source = "import a from 'a';\nimport b from 'b';\nimport c from 'c';\nimport d from 'd';\nimport e from 'e';\n\nexport function main() {}\n";
        let summary = summarize_source("sample.ts", source);
        let elided = summary
            .segments
            .iter()
            .find(|segment| segment.kind == "elided")
            .expect("import run elided");
        assert_eq!((elided.start_line, elided.end_line), (2, 4));
    }

    #[test]
    fn summarizes_markdown_fenced_code_on_host_lines() {
        let source = "# Example\n\n```rust\npub fn demo() {\n    let one = 1;\n    let two = 2;\n    let three = 3;\n    println!(\"{}\", one + two + three);\n}\n```\n";
        let summary = summarize_source("README.md", source);
        let elided = summary
            .segments
            .iter()
            .find(|segment| segment.kind == "elided")
            .expect("fenced body elided");

        assert_eq!(summary.language.as_deref(), Some("markdown"));
        assert!(summary.parsed);
        assert!(summary.elided);
        assert_eq!((elided.start_line, elided.end_line), (5, 8));
    }

    #[test]
    fn malformed_markdown_fenced_code_returns_full_content() {
        let source = "# Example\n\n```typescript\nexport function broken( {\n```\n";
        let summary = summarize_source("README.md", source);

        assert!(!summary.parsed);
        assert!(!summary.elided);
        assert_eq!(summary.segments[0].text.as_deref(), Some(source));
    }

    #[test]
    fn parse_failure_returns_full_content() {
        let source = "export function broken( {\n";
        let summary = summarize_source("sample.ts", source);
        assert!(!summary.parsed);
        assert!(!summary.elided);
        assert_eq!(summary.segments[0].text.as_deref(), Some(source));
    }
}
