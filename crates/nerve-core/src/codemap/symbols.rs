use super::block::first_descendant_kind;
use super::*;

pub(crate) fn symbols_for_path(
    source: &str,
    rel_path: &str,
) -> Result<Option<ParsedCodeFile>, String> {
    if super::markdown::is_markdown_path(rel_path) {
        return symbols_for_markdown_fences(source);
    }
    let Some(language) = Language::from_path_or_source(rel_path, source) else {
        return Ok(None);
    };
    let (symbols, references) = code_facts_for_language(language, source)?;
    Ok(Some(ParsedCodeFile {
        language: language.name().to_string(),
        symbols,
        references,
    }))
}

fn symbols_for_markdown_fences(source: &str) -> Result<Option<ParsedCodeFile>, String> {
    let mut symbols = Vec::new();
    let mut references = Vec::new();
    let mut lines: Vec<&str> = source.split_inclusive('\n').collect();
    if lines.is_empty() && !source.is_empty() {
        lines.push(source);
    }
    for fence in super::markdown::supported_fences(&lines) {
        let body = super::markdown::fence_source(&lines, &fence);
        let column_offsets = super::markdown::fence_column_offsets(&lines, &fence);
        parse_markdown_fence(
            &body,
            fence.body.start,
            &column_offsets,
            fence.language,
            &mut symbols,
            &mut references,
        )?;
    }
    if symbols.is_empty() && references.is_empty() {
        return Ok(None);
    }
    symbols.sort_by(symbol_cmp);
    references.sort_by(reference_cmp);
    Ok(Some(ParsedCodeFile {
        language: "markdown".to_string(),
        symbols,
        references,
    }))
}

fn line_column_for_byte(source: &[u8], offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut column = 1usize;
    for byte in source.iter().take(offset.min(source.len())) {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

fn symbol_cmp(a: &CodeSymbol, b: &CodeSymbol) -> std::cmp::Ordering {
    a.line
        .cmp(&b.line)
        .then(a.column.cmp(&b.column))
        .then_with(|| a.name.cmp(&b.name))
}

fn reference_cmp(a: &CodeReference, b: &CodeReference) -> std::cmp::Ordering {
    a.line
        .cmp(&b.line)
        .then(a.column.cmp(&b.column))
        .then_with(|| a.name.cmp(&b.name))
}

fn parse_markdown_fence(
    source: &str,
    line_offset: usize,
    column_offsets: &[usize],
    language: Language,
    symbols: &mut Vec<CodeSymbol>,
    references: &mut Vec<CodeReference>,
) -> Result<(), String> {
    if source.is_empty() {
        return Ok(());
    }
    let (mut block_symbols, mut block_references) = code_facts_for_language(language, source)?;
    for symbol in &mut block_symbols {
        symbol.column += column_offsets
            .get(symbol.line.saturating_sub(1))
            .copied()
            .unwrap_or_default();
        symbol.line += line_offset;
    }
    for reference in &mut block_references {
        reference.column += column_offsets
            .get(reference.line.saturating_sub(1))
            .copied()
            .unwrap_or_default();
        reference.line += line_offset;
        reference.language = Some(language.name().to_string());
    }
    symbols.extend(block_symbols);
    references.extend(block_references);
    Ok(())
}

#[cfg(fuzzing)]
#[doc(hidden)]
pub fn fuzz_symbols_for_path(
    source: &str,
    rel_path: &str,
) -> Result<Option<(String, Vec<CodeSymbol>)>, String> {
    Ok(symbols_for_path(source, rel_path)?.map(|parsed| (parsed.language, parsed.symbols)))
}

pub(super) fn code_facts_for_language(
    language: Language,
    source: &str,
) -> Result<(Vec<CodeSymbol>, Vec<CodeReference>), String> {
    let config = language
        .config()
        .ok_or_else(|| format!("no tags configuration for {}", language.name()))?;
    let mut context = TagsContext::new();
    let bytes = source.as_bytes();
    let (tags, _failed) = context
        .generate_tags(config, bytes, None)
        .map_err(|err| format!("tags error: {err:?}"))?;

    let members_by_def = members_by_definition(language, bytes);
    let mut symbols = Vec::new();
    let mut references = Vec::new();
    for tag in tags {
        let tag = tag.map_err(|err| format!("tag error: {err:?}"))?;
        let Some(name) = bytes.get(tag.name_range.clone()) else {
            continue;
        };
        let name = String::from_utf8_lossy(name).into_owned();
        let (line, column) = line_column_for_byte(bytes, tag.name_range.start);
        let kind = config.syntax_type_name(tag.syntax_type_id).to_string();
        if tag.is_definition {
            let signature = signature_for(language, bytes, &tag);
            let members = members_by_def
                .get(&(name.clone(), line))
                .cloned()
                .unwrap_or_default();
            symbols.push(CodeSymbol {
                kind,
                name,
                line,
                column,
                signature,
                members,
            });
        } else {
            references.push(CodeReference::new(kind, name, line).with_column(column));
        }
    }
    references.extend(super::imports::import_references_for_language(
        language, source,
    ));
    // Deterministic order for stable output and goldens.
    symbols.sort_by(symbol_cmp);
    references.sort_by(reference_cmp);
    Ok((symbols, references))
}

/// Build a compact one-line signature for a definition tag: text from the start
/// of the definition node up to the body (a top-level `{` or `;` for brace
/// languages, `:` for Python), with all whitespace collapsed to single spaces and
/// the length capped. Falls back to the trimmed first line tree-sitter already
/// computed (`line_range`). Returns `None` if nothing useful remains.
pub(super) fn signature_for(
    language: Language,
    source: &[u8],
    tag: &tree_sitter_tags::Tag,
) -> Option<String> {
    const MAX_SCAN: usize = 512;
    const MAX_CHARS: usize = 200;

    let start = tag.range.start.min(source.len());
    let scan_end = tag.range.end.min(start + MAX_SCAN).min(source.len());
    let node = source.get(start..scan_end)?;
    let raw = match signature_span_end(language, node) {
        Some(end) => &node[..end],
        None => {
            let lo = tag.line_range.start.min(source.len());
            let hi = tag.line_range.end.min(source.len());
            source.get(lo..hi)?
        }
    };

    let collapsed = String::from_utf8_lossy(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let trimmed = collapsed
        .trim()
        .trim_end_matches(['{', ':', ';'])
        .trim_end();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, MAX_CHARS))
}

/// Cap a string at `max` characters on a char boundary, appending an ellipsis.
pub(super) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        let capped: String = text.chars().take(max.saturating_sub(1)).collect();
        format!("{capped}\u{2026}")
    } else {
        text.to_string()
    }
}

/// Byte offset within `node` where a definition's signature ends (exclusive):
/// the first top-level body opener. Brace languages stop before `{` or at `;`;
/// Python stops at the header `:`. Bracket depth (`()`/`[]`, plus `{}` for Python)
/// keeps separators inside argument lists from ending the signature.
pub(super) fn signature_span_end(language: Language, node: &[u8]) -> Option<usize> {
    let python = matches!(language, Language::Python);
    let mut depth: i32 = 0;
    for (i, &byte) in node.iter().enumerate() {
        match byte {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'{' if python => depth += 1,
            b'}' if python => depth -= 1,
            b'{' if depth <= 0 => return Some(i),
            b';' if depth <= 0 => return Some(i),
            b':' if python && depth <= 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// Map each container definition `(name, line)` to its declared members via a
/// second tree-sitter parse (the tags API does not expose its tree). Returns an
/// empty map for languages without member support.
pub(super) fn members_by_definition(
    language: Language,
    source: &[u8],
) -> std::collections::HashMap<(String, usize), Vec<CodeMember>> {
    let mut out: std::collections::HashMap<(String, usize), Vec<CodeMember>> =
        std::collections::HashMap::new();
    let containers = language.member_containers();
    if containers.is_empty() {
        return out;
    }
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&language.ts_language()).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(source, None) else {
        return out;
    };

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if containers.contains(&node.kind())
            && let Some(name_node) = node.child_by_field_name("name")
            && let Ok(name) = name_node.utf8_text(source)
        {
            let line = name_node.start_position().row + 1;
            let members = collect_members(language, node, source);
            if !members.is_empty() {
                out.entry((name.to_string(), line)).or_insert(members);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out
}

/// The node whose direct children are a container's member declarations.
pub(super) fn field_list_node<'a>(
    language: Language,
    container: tree_sitter::Node<'a>,
) -> Option<tree_sitter::Node<'a>> {
    match (language, container.kind()) {
        (Language::Go, _) => {
            // type_spec -> (struct_type|interface_type) -> field_declaration_list
            let ty = container.child_by_field_name("type")?;
            let mut cursor = ty.walk();
            ty.children(&mut cursor)
                .find(|child| child.kind() == "field_declaration_list")
        }
        // Records carry their components in a parameter list, not a body.
        (Language::Java, "record_declaration") => container.child_by_field_name("parameters"),
        (Language::CSharp, "record_declaration") => {
            let mut cursor = container.walk();
            container
                .children(&mut cursor)
                .find(|child| child.kind() == "parameter_list")
        }
        _ => container.child_by_field_name("body"),
    }
}

pub(super) fn collect_members(
    language: Language,
    container: tree_sitter::Node,
    source: &[u8],
) -> Vec<CodeMember> {
    const MAX_MEMBERS: usize = 200;
    if matches!(language, Language::Ruby) {
        return collect_ruby_members(container, source, MAX_MEMBERS);
    }
    let Some(list) = field_list_node(language, container) else {
        return Vec::new();
    };
    let field_kinds = language.member_field_kinds();
    let mut members = Vec::new();
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        if !field_kinds.contains(&child.kind()) {
            continue;
        }
        if let Some(member) = make_member(language, child, source) {
            members.push(member);
            if members.len() >= MAX_MEMBERS {
                break;
            }
        }
    }
    members
}

/// Ruby exposes instance attributes through `attr_accessor`/`attr_reader`/
/// `attr_writer` calls rather than field declarations; each symbol argument
/// becomes a member.
pub(super) fn collect_ruby_members(
    container: tree_sitter::Node,
    source: &[u8],
    max_members: usize,
) -> Vec<CodeMember> {
    const ATTR: &[&str] = &["attr_accessor", "attr_reader", "attr_writer", "attr"];
    let Some(body) = container.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut cursor = body.walk();
    for statement in body.children(&mut cursor) {
        if statement.kind() != "call" {
            continue;
        }
        let Some(method) = statement
            .child_by_field_name("method")
            .and_then(|node| node.utf8_text(source).ok())
        else {
            continue;
        };
        if !ATTR.contains(&method) {
            continue;
        }
        let Some(arguments) = statement.child_by_field_name("arguments") else {
            continue;
        };
        let mut arg_cursor = arguments.walk();
        for argument in arguments.children(&mut arg_cursor) {
            if argument.kind() != "simple_symbol" {
                continue;
            }
            let Ok(text) = argument.utf8_text(source) else {
                continue;
            };
            let name = text.trim_start_matches(':').to_string();
            if name.is_empty() {
                continue;
            }
            members.push(CodeMember {
                signature: Some(format!("{method} :{name}")),
                name,
            });
            if members.len() >= max_members {
                return members;
            }
        }
    }
    members
}

pub(super) fn make_member(
    language: Language,
    field: tree_sitter::Node,
    source: &[u8],
) -> Option<CodeMember> {
    let name = member_name(language, field, source)?;
    let signature = member_signature(field, source);
    Some(CodeMember { name, signature })
}

pub(super) fn member_name(
    language: Language,
    field: tree_sitter::Node,
    source: &[u8],
) -> Option<String> {
    if matches!(language, Language::Python) {
        let assignment = field.named_child(0)?;
        if assignment.kind() != "assignment" {
            return None;
        }
        let left = assignment.child_by_field_name("left")?;
        if left.kind() != "identifier" {
            return None;
        }
        return left.utf8_text(source).ok().map(str::to_owned);
    }
    let node = first_descendant_kind(field, language.member_name_kinds())?;
    node.utf8_text(source).ok().map(str::to_owned)
}

pub(super) fn member_signature(field: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let text = field.utf8_text(source).ok()?;
    let first_line = text.lines().next().unwrap_or(text);
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = collapsed.split('{').next().unwrap_or(collapsed.as_str());
    let trimmed = cut.trim().trim_end_matches([';', ',']).trim_end();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_chars(trimmed, 120))
}
