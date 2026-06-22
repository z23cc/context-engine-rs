use super::*;

/// First descendant (document order) whose kind is in `kinds`.
pub(super) fn first_descendant_kind<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if kinds.contains(&child.kind()) {
            return Some(child);
        }
        if let Some(found) = first_descendant_kind(child, kinds) {
            return Some(found);
        }
    }
    None
}

/// Resolve the syntactic block beginning on 1-based `start_line` using
/// tree-sitter: the largest named node that starts on that line (so a `def`/`fn`
/// opener selects the whole construct, and pointing at a leading decorator sweeps
/// it in too). Returns an inclusive 1-based `(start, end)` range, or `None` when
/// the language is unsupported or nothing begins there — callers then fall back
/// to a brace/indentation heuristic.
pub(crate) fn block_span(path: &str, source: &str, start_line: usize) -> Option<(usize, usize)> {
    let language = Language::from_path_or_source(path, source)?;
    block_span_for_language(language, source, start_line)
}

pub(super) fn block_span_for_language(
    language: Language,
    source: &str,
    start_line: usize,
) -> Option<(usize, usize)> {
    let target_row = start_line.checked_sub(1)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language.ts_language()).ok()?;
    let tree = parser.parse(source, None)?;

    // Seed from the root's children so the whole-file root node (which also
    // starts on line 1) is never chosen as the block.
    let root = tree.root_node();
    let mut best: Option<tree_sitter::Node> = None;
    let mut stack: Vec<tree_sitter::Node> = Vec::new();
    let mut root_cursor = root.walk();
    for child in root.children(&mut root_cursor) {
        if child.start_position().row <= target_row && child.end_position().row >= target_row {
            stack.push(child);
        }
    }
    while let Some(node) = stack.pop() {
        if node.is_named()
            && node.start_position().row == target_row
            && best.is_none_or(|current| node.end_byte() > current.end_byte())
        {
            best = Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.start_position().row <= target_row && child.end_position().row >= target_row {
                stack.push(child);
            }
        }
    }

    let node = best?;
    let mut end_row = node.end_position().row;
    // A node ending at column 0 closes on the previous line, not the next.
    if node.end_position().column == 0 && end_row > target_row {
        end_row -= 1;
    }
    Some((start_line, end_row + 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContainingBlockError {
    UnsupportedLanguage,
    ParseError,
    BlankLine,
}

pub(crate) fn containing_block_span(
    path: &str,
    source: &str,
    first_line: usize,
    last_line: usize,
) -> Result<Option<(usize, usize)>, ContainingBlockError> {
    let language = Language::from_path_or_source(path, source)
        .ok_or(ContainingBlockError::UnsupportedLanguage)?;
    containing_block_span_for_language(language, source, first_line, last_line)
}

pub(super) fn containing_block_span_for_language(
    language: Language,
    source: &str,
    first_line: usize,
    last_line: usize,
) -> Result<Option<(usize, usize)>, ContainingBlockError> {
    if first_line == 0 || last_line < first_line || line_text(source, first_line).is_none() {
        return Ok(None);
    }
    if line_text(source, first_line).is_some_and(|line| line.trim().is_empty()) {
        return Err(ContainingBlockError::BlankLine);
    }
    let tree = parse_clean_tree(language, source)?;
    Ok(smallest_containing_block(
        tree.root_node(),
        first_line - 1,
        last_line - 1,
    ))
}

fn parse_clean_tree(
    language: Language,
    source: &str,
) -> Result<tree_sitter::Tree, ContainingBlockError> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts_language())
        .map_err(|_| ContainingBlockError::ParseError)?;
    let tree = parser
        .parse(source, None)
        .ok_or(ContainingBlockError::ParseError)?;
    if tree.root_node().has_error() {
        return Err(ContainingBlockError::ParseError);
    }
    Ok(tree)
}

fn smallest_containing_block(
    root: tree_sitter::Node<'_>,
    first_row: usize,
    last_row: usize,
) -> Option<(usize, usize)> {
    let mut best: Option<tree_sitter::Node<'_>> = None;
    let mut stack = Vec::new();
    let mut root_cursor = root.walk();
    stack.extend(root.children(&mut root_cursor));
    while let Some(node) = stack.pop() {
        if !node_contains_rows(node, first_row, last_row) {
            continue;
        }
        if node.is_named() && node_start_line(node) < node_end_line(node) {
            best = better_containing_node(best, node);
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    best.map(|node| (node_start_line(node), node_end_line(node)))
}

fn better_containing_node<'a>(
    current: Option<tree_sitter::Node<'a>>,
    candidate: tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    let Some(current) = current else {
        return Some(candidate);
    };
    let current_lines = node_end_line(current) - node_start_line(current);
    let candidate_lines = node_end_line(candidate) - node_start_line(candidate);
    if candidate_lines < current_lines
        || (candidate_lines == current_lines
            && candidate.byte_range().len() < current.byte_range().len())
    {
        Some(candidate)
    } else {
        Some(current)
    }
}

fn node_contains_rows(node: tree_sitter::Node<'_>, first_row: usize, last_row: usize) -> bool {
    node.start_position().row <= first_row && node_content_end_row(node) >= last_row
}

fn node_start_line(node: tree_sitter::Node<'_>) -> usize {
    node.start_position().row + 1
}

fn node_end_line(node: tree_sitter::Node<'_>) -> usize {
    node_content_end_row(node) + 1
}

fn node_content_end_row(node: tree_sitter::Node<'_>) -> usize {
    let pos = node.end_position();
    if pos.column == 0 && pos.row > 0 {
        pos.row - 1
    } else {
        pos.row
    }
}

fn line_text(source: &str, line: usize) -> Option<&str> {
    source.split_inclusive('\n').nth(line.checked_sub(1)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containing_block_chooses_smallest_nested_node() {
        let source = "fn outer() {\n    if true {\n        println!(\"x\");\n    }\n}\n";
        let span = containing_block_span("lib.rs", source, 3, 3)
            .expect("parse")
            .expect("span");
        assert_eq!(span, (2, 4));
    }

    #[test]
    fn containing_block_excludes_whole_file_root() {
        let source = "fn one() {\n}\n\nfn two() {\n}\n";
        let span = containing_block_span("lib.rs", source, 2, 4).expect("parse");
        assert_eq!(span, None);
    }

    #[test]
    fn containing_block_out_of_range_is_none() {
        let source = "fn one() {\n}\n";
        let span = containing_block_span("lib.rs", source, 99, 99).expect("parse");
        assert_eq!(span, None);
    }

    #[test]
    fn shebang_python_block_span_without_extension() {
        let source = "#!/usr/bin/env python3\ndef main():\n    print('x')\n";
        assert_eq!(block_span("script", source, 2), Some((2, 3)));
        let containing = containing_block_span("script", source, 3, 3)
            .expect("parse")
            .expect("span");
        assert_eq!(containing, (2, 3));
    }
}

/// A tree-sitter syntax problem in a file (used to flag broken edits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxIssue {
    pub line: usize,
    pub message: String,
}

pub(super) const MAX_SYNTAX_ISSUES: usize = 20;

/// Re-parse `source` and report tree-sitter `ERROR` / missing nodes as syntax
/// diagnostics. Empty for unsupported languages or a clean parse. Syntax-level
/// only — this is not type checking or a language server.
pub(super) fn syntax_diagnostics(path: &str, source: &str) -> Vec<SyntaxIssue> {
    let Some(language) = Language::from_path_or_source(path, source) else {
        return Vec::new();
    };
    syntax_diagnostics_for_language(language, source, 0, None)
}

pub(super) fn syntax_diagnostics_for_language(
    language: Language,
    source: &str,
    line_offset: usize,
    message_prefix: Option<&str>,
) -> Vec<SyntaxIssue> {
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };
    if !tree.root_node().has_error() {
        return Vec::new();
    }

    let mut issues = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_missing() {
            issues.push(SyntaxIssue {
                line: node.start_position().row + 1 + line_offset,
                message: diagnostic_message(format!("missing {}", node.kind()), message_prefix),
            });
        } else if node.is_error() {
            issues.push(SyntaxIssue {
                line: node.start_position().row + 1 + line_offset,
                message: diagnostic_message("syntax error", message_prefix),
            });
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.has_error() {
                stack.push(child);
            }
        }
    }
    issues.sort_by_key(|issue| issue.line);
    issues.dedup();
    issues.truncate(MAX_SYNTAX_ISSUES);
    issues
}

fn diagnostic_message(message: impl Into<String>, prefix: Option<&str>) -> String {
    let message = message.into();
    prefix.map_or(message.clone(), |prefix| format!("{prefix}: {message}"))
}
