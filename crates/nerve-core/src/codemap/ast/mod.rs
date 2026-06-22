use super::symbols::truncate_chars;
use super::*;
use std::ops::Range;

mod pattern;

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
    let language = Language::from_path_or_source(path, source)
        .ok_or_else(|| "unsupported language".to_string())?;
    run_ast_search(
        language,
        source,
        query_src,
        max,
        &BTreeMap::new(),
        None,
        false,
    )
}

pub(crate) fn ast_search_language(
    language_name: &str,
    source: &str,
    query_src: &str,
    max: usize,
) -> Result<Vec<AstMatch>, String> {
    let language =
        Language::from_name(language_name).ok_or_else(|| "unsupported language".to_string())?;
    run_ast_search(
        language,
        source,
        query_src,
        max,
        &BTreeMap::new(),
        None,
        false,
    )
}

pub(crate) fn ast_search_pattern(
    path: &str,
    source: &str,
    pattern: &str,
    max: usize,
) -> Result<Vec<AstMatch>, String> {
    let language = Language::from_path_or_source(path, source)
        .ok_or_else(|| "unsupported language".to_string())?;
    ast_search_pattern_with_language(language, source, pattern, max)
}

pub(crate) fn ast_search_pattern_language(
    language_name: &str,
    source: &str,
    pattern: &str,
    max: usize,
) -> Result<Vec<AstMatch>, String> {
    let language =
        Language::from_name(language_name).ok_or_else(|| "unsupported language".to_string())?;
    ast_search_pattern_with_language(language, source, pattern, max)
}

fn ast_search_pattern_with_language(
    language: Language,
    source: &str,
    pattern: &str,
    max: usize,
) -> Result<Vec<AstMatch>, String> {
    let compiled = pattern::compile_pattern_query_for_language(language, pattern)?;
    run_ast_search(
        language,
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
    let language = Language::from_path_or_source(path, source)
        .ok_or_else(|| "unsupported language".to_string())?;
    let compiled = pattern::compile_pattern_query_for_language(language, pattern)?;
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
    language: Language,
    source: &str,
    query_src: &str,
    max: usize,
    literals: &BTreeMap<String, String>,
    shape: Option<&PatternShape>,
    pattern_mode: bool,
) -> Result<Vec<AstMatch>, String> {
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
        if shape.is_some_and(|shape| !pattern::shape_matches(shape, node, bytes)) {
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
    let language = Language::from_path_or_source(path, source)
        .ok_or_else(|| "unsupported language".to_string())?;
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
        if shape.is_some_and(|shape| !pattern::shape_matches(shape, region, bytes)) {
            continue;
        }
        edits.push((
            region.byte_range(),
            pattern::render_template(replacement, &values),
        ));
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
