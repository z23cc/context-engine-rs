use super::symbols::truncate_chars;
use super::*;
use std::ops::Range;

const META_PREFIX: &str = "__ctx_meta_";
const LITERAL_PREFIX: &str = "__ctx_lit";

/// One structural match from [`ast_search`]: the `@match` region's line and
/// text, plus any other captures as metavariables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AstMatch {
    pub line: usize,
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub captures: BTreeMap<String, String>,
}

struct PatternQuery {
    query: String,
    literals: BTreeMap<String, String>,
    shape: PatternShape,
}

#[derive(Debug, Clone)]
enum PatternShape {
    Meta,
    Node {
        kind: String,
        text: Option<String>,
        children: Vec<PatternShapeChild>,
    },
}

#[derive(Debug, Clone)]
struct PatternShapeChild {
    field: Option<String>,
    shape: PatternShape,
}

struct WrappedPattern {
    source: String,
    range: Range<usize>,
}

/// Whether `language_name` maps to a supported tree-sitter grammar.
pub(crate) fn ast_language_supported(language_name: &str) -> bool {
    Language::from_name(language_name).is_some()
}

/// Display language name for a path's extension, if supported.
pub(crate) fn path_language_name(path: &str) -> Option<&'static str> {
    Language::from_path(path).map(Language::name)
}

pub(super) fn ast_node_text(node: tree_sitter::Node, source: &[u8]) -> String {
    let text = node.utf8_text(source).unwrap_or_default();
    let first = text.lines().next().unwrap_or(text);
    truncate_chars(first, 160)
}

/// Run a tree-sitter query over one source file. The `@match` capture (or, if
/// absent, the largest captured node) marks each result; other captures are
/// returned as metavariables. Errors if the language is unsupported or the query
/// is invalid for it.
pub(crate) fn ast_search(
    path: &str,
    source: &str,
    query_src: &str,
    max: usize,
) -> Result<Vec<AstMatch>, String> {
    run_ast_search(path, source, query_src, max, &BTreeMap::new(), None, false)
}

pub(crate) fn ast_search_pattern(
    path: &str,
    source: &str,
    pattern: &str,
    max: usize,
) -> Result<Vec<AstMatch>, String> {
    let compiled = compile_pattern_query(path, pattern)?;
    run_ast_search(
        path,
        source,
        &compiled.query,
        max,
        &compiled.literals,
        Some(&compiled.shape),
        true,
    )
}

/// Replace every `@match` region matched by `query_src` with `replacement`,
/// where `${capture}` placeholders are substituted by captured text. Returns the
/// new source and rewrite count. Applied bottom-up so byte offsets stay valid;
/// overlapping (nested) matches keep the outermost.
pub(crate) fn ast_rewrite(
    path: &str,
    source: &str,
    query_src: &str,
    replacement: &str,
) -> Result<(String, usize), String> {
    run_ast_rewrite(
        path,
        source,
        query_src,
        replacement,
        &BTreeMap::new(),
        None,
        false,
    )
}

pub(crate) fn ast_rewrite_pattern(
    path: &str,
    source: &str,
    pattern: &str,
    replacement: &str,
) -> Result<(String, usize), String> {
    let compiled = compile_pattern_query(path, pattern)?;
    run_ast_rewrite(
        path,
        source,
        &compiled.query,
        replacement,
        &compiled.literals,
        Some(&compiled.shape),
        true,
    )
}

fn run_ast_search(
    path: &str,
    source: &str,
    query_src: &str,
    max: usize,
    literals: &BTreeMap<String, String>,
    shape: Option<&PatternShape>,
    pattern_mode: bool,
) -> Result<Vec<AstMatch>, String> {
    let language = Language::from_path(path).ok_or_else(|| "unsupported language".to_string())?;
    let query = build_query(language, query_src)?;
    let Some(tree) = parse_source(language, source)? else {
        return Ok(Vec::new());
    };
    let names = query.capture_names();
    let bytes = source.as_bytes();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), bytes);
    let mut matches = Vec::new();
    while let Some(query_match) = iter.next() {
        let Some((captures, node)) = capture_match(
            query_match.captures,
            names,
            bytes,
            literals,
            pattern_mode,
            false,
        ) else {
            continue;
        };
        let Some(node) = node.or_else(|| largest_capture_node(query_match.captures)) else {
            continue;
        };
        if shape.is_some_and(|shape| !shape_matches(shape, node, bytes)) {
            continue;
        }
        matches.push(AstMatch {
            line: node.start_position().row + 1,
            text: ast_node_text(node, bytes),
            captures,
        });
        if matches.len() >= max {
            break;
        }
    }
    Ok(matches)
}

fn run_ast_rewrite(
    path: &str,
    source: &str,
    query_src: &str,
    replacement: &str,
    literals: &BTreeMap<String, String>,
    shape: Option<&PatternShape>,
    pattern_mode: bool,
) -> Result<(String, usize), String> {
    let language = Language::from_path(path).ok_or_else(|| "unsupported language".to_string())?;
    let query = build_query(language, query_src)?;
    if !query.capture_names().contains(&"match") {
        return Err("rewrite query must capture the region to replace as @match".to_string());
    }
    let Some(tree) = parse_source(language, source)? else {
        return Ok((source.to_string(), 0));
    };
    let bytes = source.as_bytes();
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), bytes);
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    while let Some(query_match) = iter.next() {
        let Some((values, region)) =
            rewrite_match(query_match.captures, names, bytes, literals, pattern_mode)
        else {
            continue;
        };
        if shape.is_some_and(|shape| !shape_matches(shape, region, bytes)) {
            continue;
        }
        edits.push((region.byte_range(), render_template(replacement, &values)));
    }
    Ok(apply_rewrites(source, edits))
}

fn build_query(language: Language, query_src: &str) -> Result<tree_sitter::Query, String> {
    if query_src.contains("(#") {
        return Err(
            "query predicates such as (#eq? ...) are not applied yet; use a structural pattern"
                .to_string(),
        );
    }
    tree_sitter::Query::new(&language.ts_language(), query_src)
        .map_err(|err| format!("invalid query: {err}"))
}

fn parse_source(language: Language, source: &str) -> Result<Option<tree_sitter::Tree>, String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&language.ts_language())
        .map_err(|err| format!("language error: {err}"))?;
    Ok(parser.parse(source, None))
}

fn capture_match<'a>(
    captures: &[tree_sitter::QueryCapture<'a>],
    names: &[&str],
    bytes: &'a [u8],
    literals: &BTreeMap<String, String>,
    pattern_mode: bool,
    full_values: bool,
) -> Option<(BTreeMap<String, String>, Option<tree_sitter::Node<'a>>)> {
    let mut values = BTreeMap::new();
    let mut seen_full = BTreeMap::new();
    let mut match_node = None;
    for capture in captures {
        let name = names[capture.index as usize];
        let text = capture.node.utf8_text(bytes).unwrap_or_default();
        if pattern_mode && name.starts_with(LITERAL_PREFIX) {
            if literals.get(name).is_none_or(|expected| expected != text) {
                return None;
            }
            continue;
        }
        if pattern_mode && !record_pattern_capture(name, text, &mut seen_full) {
            return None;
        }
        let value = if full_values {
            text.to_string()
        } else {
            ast_node_text_for_capture(text)
        };
        values.insert(name.to_string(), value);
        if name == "match" {
            match_node = Some(capture.node);
        }
    }
    Some((values, match_node))
}

fn record_pattern_capture(name: &str, text: &str, seen: &mut BTreeMap<String, String>) -> bool {
    match seen.get(name) {
        Some(existing) => existing == text,
        None => {
            seen.insert(name.to_string(), text.to_string());
            true
        }
    }
}

fn ast_node_text_for_capture(text: &str) -> String {
    truncate_chars(text.lines().next().unwrap_or(text), 160)
}

fn largest_capture_node<'a>(
    captures: &[tree_sitter::QueryCapture<'a>],
) -> Option<tree_sitter::Node<'a>> {
    captures
        .iter()
        .map(|capture| capture.node)
        .max_by_key(|node| node.byte_range().len())
}

fn rewrite_match<'a>(
    captures: &[tree_sitter::QueryCapture<'a>],
    names: &[&str],
    bytes: &'a [u8],
    literals: &BTreeMap<String, String>,
    pattern_mode: bool,
) -> Option<(BTreeMap<String, String>, tree_sitter::Node<'a>)> {
    let (values, region) = capture_match(captures, names, bytes, literals, pattern_mode, true)?;
    region.map(|node| (values, node))
}

fn apply_rewrites(source: &str, mut edits: Vec<(Range<usize>, String)>) -> (String, usize) {
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.0.start));
    let mut result = source.to_string();
    let mut boundary = result.len();
    let mut count = 0;
    for (range, rendered) in edits {
        if range.end > boundary {
            continue;
        }
        result.replace_range(range.clone(), &rendered);
        boundary = range.start;
        count += 1;
    }
    (result, count)
}

fn compile_pattern_query(path: &str, pattern: &str) -> Result<PatternQuery, String> {
    let language = Language::from_path(path).ok_or_else(|| "unsupported language".to_string())?;
    let prepared = replace_metavars(pattern)?;
    let wrapped = wrap_pattern(language, &prepared);
    let Some(tree) = parse_source(language, &wrapped.source)? else {
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
            "class __CtxPatternWrapper { void f() { ",
            if semi.is_empty() { " } }" } else { "; } }" },
        ),
        Language::C | Language::Cpp => (
            "void __ctx_pattern_wrapper() { ",
            if semi.is_empty() { " }" } else { "; }" },
        ),
        Language::CSharp => (
            "class __CtxPatternWrapper { void F() { ",
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

fn shape_matches(shape: &PatternShape, node: tree_sitter::Node<'_>, bytes: &[u8]) -> bool {
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
