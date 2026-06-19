use std::collections::BTreeMap;
use std::ops::Range;

use super::super::language::Language;
use super::{
    LITERAL_PREFIX, META_PREFIX, PatternQuery, PatternShape, PatternShapeChild, WrappedPattern,
};

pub(super) fn compile_pattern_query(path: &str, pattern: &str) -> Result<PatternQuery, String> {
    let language = Language::from_path(path).ok_or_else(|| "unsupported language".to_string())?;
    let prepared = replace_metavars(pattern)?;
    let wrapped = wrap_pattern(language, &prepared);
    let Some(tree) = super::parse_source(language, &wrapped.source)? else {
        return Err("pattern did not parse".to_string());
    };
    let bytes = wrapped.source.as_bytes();
    let Some(node) = best_pattern_node(tree.root_node(), &wrapped.range) else {
        return Err("pattern did not contain a searchable syntax node".to_string());
    };
    if node.has_error() || node.is_missing() {
        return Err("pattern did not parse cleanly".to_string());
    }
    let mut literals = BTreeMap::new();
    let shape = pattern_shape(node, bytes);
    let query = format!("{} @match", node_query(node, bytes, &mut literals));
    Ok(PatternQuery {
        query,
        literals,
        shape,
    })
}

fn replace_metavars(pattern: &str) -> Result<String, String> {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let mut name = String::new();
        while let Some((_, next)) = chars.peek().copied() {
            if next == '_' || next.is_ascii_alphanumeric() {
                name.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            return Err("pattern metavariable `$` must be followed by a name".to_string());
        }
        out.push_str(META_PREFIX);
        out.push_str(&name);
    }
    Ok(out)
}

fn wrap_pattern(language: Language, pattern: &str) -> WrappedPattern {
    let (prefix, suffix) = pattern_wrapper(language, pattern);
    let source = format!("{prefix}{pattern}{suffix}");
    let range = prefix.len()..prefix.len() + pattern.len();
    WrappedPattern { source, range }
}

fn pattern_wrapper(language: Language, pattern: &str) -> (&'static str, &'static str) {
    let semi = if pattern.trim_end().ends_with(';') {
        ""
    } else {
        ";"
    };
    match language {
        Language::Rust => (
            "fn __ctx_pattern_wrapper() { ",
            if semi.is_empty() { " }" } else { "; }" },
        ),
        Language::JavaScript | Language::TypeScript | Language::Tsx => (
            "function __ctx_pattern_wrapper() { ",
            if semi.is_empty() { " }" } else { "; }" },
        ),
        Language::Go => ("package main\nfunc __ctxPatternWrapper() { ", "\n}\n"),
        Language::Java => (
            "class __NervePatternWrapper { void f() { ",
            if semi.is_empty() { " } }" } else { "; } }" },
        ),
        Language::C | Language::Cpp => (
            "void __ctx_pattern_wrapper() { ",
            if semi.is_empty() { " }" } else { "; }" },
        ),
        Language::CSharp => (
            "class __NervePatternWrapper { void F() { ",
            if semi.is_empty() { " } }" } else { "; } }" },
        ),
        Language::Php => (
            "<?php function __ctx_pattern_wrapper() { ",
            if semi.is_empty() { " }" } else { "; }" },
        ),
        Language::Python => ("def __ctx_pattern_wrapper():\n    ", "\n"),
        Language::Ruby => ("def __ctx_pattern_wrapper\n  ", "\nend\n"),
    }
}

fn best_pattern_node<'a>(
    root: tree_sitter::Node<'a>,
    range: &Range<usize>,
) -> Option<tree_sitter::Node<'a>> {
    let mut best = None;
    collect_best_node(root, range, &mut best);
    best
}

fn collect_best_node<'a>(
    node: tree_sitter::Node<'a>,
    range: &Range<usize>,
    best: &mut Option<tree_sitter::Node<'a>>,
) {
    if node.is_named() && range_contains_node(range, node) {
        let replace =
            best.is_none_or(|current| node.byte_range().len() > current.byte_range().len());
        if replace {
            *best = Some(node);
        }
    }
    for index in 0..node.child_count() {
        if let Some(child) = node.child(index as u32) {
            collect_best_node(child, range, best);
        }
    }
}

fn range_contains_node(range: &Range<usize>, node: tree_sitter::Node<'_>) -> bool {
    let node_range = node.byte_range();
    node_range.start >= range.start && node_range.end <= range.end
}

fn pattern_shape(node: tree_sitter::Node<'_>, bytes: &[u8]) -> PatternShape {
    let text = node.utf8_text(bytes).unwrap_or_default();
    if text.starts_with(META_PREFIX) {
        return PatternShape::Meta;
    }
    let children = comparable_child_shapes(node, bytes);
    let text = children.is_empty().then(|| text.to_string());
    PatternShape::Node {
        kind: node.kind().to_string(),
        text,
        children,
    }
}

fn comparable_child_shapes(node: tree_sitter::Node<'_>, bytes: &[u8]) -> Vec<PatternShapeChild> {
    let mut children = Vec::new();
    for index in 0..node.child_count() {
        let Some(child) = node.child(index as u32) else {
            continue;
        };
        if child.is_extra() {
            continue;
        }
        children.push(PatternShapeChild {
            field: node.field_name_for_child(index as u32).map(str::to_string),
            shape: pattern_shape(child, bytes),
        });
    }
    children
}

pub(super) fn shape_matches(
    shape: &PatternShape,
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
) -> bool {
    match shape {
        PatternShape::Meta => true,
        PatternShape::Node {
            kind,
            text,
            children,
        } => {
            kind == node.kind()
                && text_matches(text, node, bytes)
                && children_match(children, node, bytes)
        }
    }
}

fn text_matches(text: &Option<String>, node: tree_sitter::Node<'_>, bytes: &[u8]) -> bool {
    text.as_ref()
        .is_none_or(|expected| node.utf8_text(bytes).unwrap_or_default() == expected)
}

fn children_match(
    expected: &[PatternShapeChild],
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
) -> bool {
    let actual = comparable_child_nodes(node);
    expected.len() == actual.len()
        && expected
            .iter()
            .zip(actual)
            .all(|(expected, actual)| child_matches(expected, actual, bytes))
}

fn comparable_child_nodes(
    node: tree_sitter::Node<'_>,
) -> Vec<(Option<String>, tree_sitter::Node<'_>)> {
    let mut children = Vec::new();
    for index in 0..node.child_count() {
        let Some(child) = node.child(index as u32) else {
            continue;
        };
        if child.is_extra() {
            continue;
        }
        children.push((
            node.field_name_for_child(index as u32).map(str::to_string),
            child,
        ));
    }
    children
}

fn child_matches(
    expected: &PatternShapeChild,
    actual: (Option<String>, tree_sitter::Node<'_>),
    bytes: &[u8],
) -> bool {
    expected.field == actual.0 && shape_matches(&expected.shape, actual.1, bytes)
}

fn node_query(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    literals: &mut BTreeMap<String, String>,
) -> String {
    let text = node.utf8_text(bytes).unwrap_or_default();
    if let Some(name) = text.strip_prefix(META_PREFIX) {
        return format!("(_) @{name}");
    }
    let children = named_child_queries(node, bytes, literals);
    if children.is_empty() {
        let capture = format!("{LITERAL_PREFIX}{}", literals.len());
        literals.insert(capture.clone(), text.to_string());
        return format!("({}) @{capture}", node.kind());
    }
    format!("({}\n{})", node.kind(), children.join("\n"))
}

fn named_child_queries(
    node: tree_sitter::Node<'_>,
    bytes: &[u8],
    literals: &mut BTreeMap<String, String>,
) -> Vec<String> {
    let mut children = Vec::new();
    for index in 0..node.child_count() {
        let Some(child) = node.child(index as u32) else {
            continue;
        };
        if !child.is_named() {
            continue;
        }
        let query = node_query(child, bytes, literals);
        let prefixed = match node.field_name_for_child(index as u32) {
            Some(field) => format!("  {field}: {query}"),
            None => format!("  {query}"),
        };
        children.push(prefixed);
    }
    children
}

pub(super) fn render_template(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(idx) = rest.find("${") {
        out.push_str(&rest[..idx]);
        rest = &rest[idx + 2..];
        match rest.find('}') {
            Some(end) => {
                if let Some(value) = values.get(&rest[..end]) {
                    out.push_str(value);
                }
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str("${");
                break;
            }
        }
    }
    out.push_str(rest);
    out
}
