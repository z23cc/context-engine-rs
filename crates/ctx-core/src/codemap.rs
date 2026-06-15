//! Multi-language codemap extraction via tree-sitter tag queries.
//!
//! One engine (tree-sitter) plus each grammar's `tags.scm` query produces
//! definitions (codemap symbols) and references (consumed by repo-map). Adding a
//! language is a grammar crate + its tags query. Note: tree-sitter grammars are
//! C, so a C toolchain is required at build time.

use crate::{models::*, port::CatalogProvider, snapshot::CatalogSnapshot};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::OnceLock,
};
use tree_sitter::StreamingIterator;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

/// A symbol (definition) extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
    /// Declaration signature (e.g. `pub fn foo(a: A) -> B`), body excluded.
    /// `None` when extraction yields nothing useful.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Fields / properties / enum variants declared directly inside a type
    /// (struct, class, interface, enum). Empty for non-container symbols.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<CodeMember>,
}

/// A field, property, or enum variant declared inside a container symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeMember {
    pub name: String,
    /// One-line declaration (e.g. `pub x: i32`), without any body. `None` when
    /// nothing useful could be extracted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// An AST-derived reference occurrence used internally by repo-map.
///
/// References are name-level occurrences (calls, etc.) captured by the tags
/// query. They do not perform scope/type/alias resolution; repo-map resolves
/// them later by same-language name matching against definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeReference {
    pub kind: String,
    pub name: String,
    pub line: usize,
    pub import_path: Option<String>,
}

/// Parsed code facts for one source file. Public codemap responses expose only
/// `symbols`; repo-map consumes `references` from the same parse result so files
/// are not parsed twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCodeFile {
    pub language: String,
    pub symbols: Vec<CodeSymbol>,
    pub references: Vec<CodeReference>,
}

/// Symbols for one cataloged file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCodeStructure {
    pub path: String,
    pub language: String,
    pub symbols: Vec<CodeSymbol>,
}

/// Non-fatal codemap diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeStructureDiagnostic {
    pub path: Option<String>,
    pub message: String,
}

/// Response for `get_code_structure`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeStructureResponse {
    pub files: Vec<FileCodeStructure>,
    pub diagnostics: Vec<CodeStructureDiagnostic>,
    pub omitted: usize,
}

/// Extract code structure for selected paths.
///
/// Empty `paths` means the whole catalog. Directory paths select entries by
/// prefix; file paths select exact entries. Unsupported files are omitted.
pub fn get_code_structure<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    paths: &[PathBuf],
) -> Result<CodeStructureResponse, CtxError> {
    let selected = select_entries(snapshot, paths);
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    let mut omitted = 0usize;

    for entry in selected {
        match provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? {
            Ok(Some(parsed)) => files.push(FileCodeStructure {
                path: entry.rel_path.clone(),
                language: parsed.language.clone(),
                symbols: parsed.symbols.clone(),
            }),
            Ok(None) => omitted += 1,
            Err(message) => diagnostics.push(CodeStructureDiagnostic {
                path: Some(entry.rel_path.clone()),
                message,
            }),
        }
    }

    Ok(CodeStructureResponse {
        files,
        diagnostics,
        omitted,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
}

/// Extra tags-query patterns for containers the bundled Java/PHP grammar queries
/// omit, so they surface as codemap symbols (and gain member expansion).
const JAVA_EXTRA_TAGS: &str = concat!(
    "(enum_declaration name: (identifier) @name) @definition.class\n",
    "(record_declaration name: (identifier) @name) @definition.class"
);
const PHP_EXTRA_TAGS: &str = "(enum_declaration name: (name) @name) @definition.class";

/// The bundled tree-sitter-c-sharp tags query does not compile against this
/// grammar version, so we supply a minimal working one.
const CSHARP_TAGS: &str = concat!(
    "(class_declaration name: (identifier) @name) @definition.class\n",
    "(struct_declaration name: (identifier) @name) @definition.class\n",
    "(interface_declaration name: (identifier) @name) @definition.interface\n",
    "(enum_declaration name: (identifier) @name) @definition.class\n",
    "(record_declaration name: (identifier) @name) @definition.class\n",
    "(method_declaration name: (identifier) @name) @definition.method\n",
    "(property_declaration name: (identifier) @name) @definition.method"
);

impl Language {
    fn from_path(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        Some(match ext {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "js" | "jsx" | "mjs" | "cjs" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" | "h" => Self::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Self::Cpp,
            "cs" => Self::CSharp,
            "rb" => Self::Ruby,
            "php" | "phtml" => Self::Php,
            _ => return None,
        })
    }

    /// Display language tag for the response `language` field. Repo-map keeps
    /// JS/TS/TSX in one resolution family separately (see repomap language_family).
    fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::Go => "go",
            Self::Java => "java",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Ruby => "ruby",
            Self::Php => "php",
        }
    }

    /// Resolve a language by its display name (the inverse of `name`).
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "rust" => Self::Rust,
            "python" => Self::Python,
            "javascript" => Self::JavaScript,
            "typescript" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            "java" => Self::Java,
            "c" => Self::C,
            "cpp" => Self::Cpp,
            "csharp" => Self::CSharp,
            "ruby" => Self::Ruby,
            "php" => Self::Php,
            _ => return None,
        })
    }

    /// Raw grammar handle for a second parse used by member extraction (the
    /// tags API does not expose its tree).
    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        }
    }

    /// Node kinds that define a type whose members we expand. Empty disables
    /// member extraction for the language.
    fn member_containers(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["struct_item", "enum_item"],
            Self::Python => &["class_definition"],
            Self::JavaScript => &["class_declaration"],
            Self::TypeScript | Self::Tsx => &["class_declaration", "interface_declaration"],
            Self::Go => &["type_spec"],
            Self::Java => &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "record_declaration",
            ],
            Self::C => &["struct_specifier"],
            Self::Cpp => &["class_specifier", "struct_specifier"],
            Self::CSharp => &[
                "class_declaration",
                "struct_declaration",
                "interface_declaration",
                "record_declaration",
                "enum_declaration",
            ],
            Self::Php => &[
                "class_declaration",
                "interface_declaration",
                "trait_declaration",
                "enum_declaration",
            ],
            Self::Ruby => &["class", "module"],
        }
    }

    /// Node kinds of member declarations inside a container's body list.
    fn member_field_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["field_declaration", "enum_variant"],
            Self::Python => &["expression_statement"],
            Self::JavaScript => &["field_definition"],
            Self::TypeScript | Self::Tsx => &["property_signature", "public_field_definition"],
            Self::Go => &["field_declaration"],
            Self::Java => &[
                "field_declaration",
                "constant_declaration",
                "enum_constant",
                "formal_parameter",
            ],
            Self::C => &["field_declaration"],
            Self::Cpp => &["field_declaration"],
            Self::CSharp => &[
                "field_declaration",
                "property_declaration",
                "enum_member_declaration",
                "parameter",
            ],
            Self::Php => &["property_declaration", "const_declaration", "enum_case"],
            Self::Ruby => &[],
        }
    }

    /// Node kinds carrying a member's name; the first match in document order
    /// wins. Python resolves its name separately (assignment target).
    fn member_name_kinds(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["field_identifier", "identifier"],
            Self::JavaScript => &["property_identifier", "private_property_identifier"],
            Self::TypeScript | Self::Tsx => &["property_identifier"],
            Self::Go => &["field_identifier"],
            Self::Java => &["identifier"],
            Self::C | Self::Cpp => &["field_identifier"],
            Self::CSharp => &["identifier"],
            Self::Php => &["variable_name", "name"],
            Self::Python | Self::Ruby => &[],
        }
    }

    /// Cached tags configuration (compiling a query is expensive).
    fn config(self) -> Option<&'static TagsConfiguration> {
        fn build(language: tree_sitter::Language, query: &str) -> Option<TagsConfiguration> {
            TagsConfiguration::new(language, query, "").ok()
        }
        macro_rules! cached {
            ($lang:expr, $query:expr) => {{
                static CELL: OnceLock<Option<TagsConfiguration>> = OnceLock::new();
                CELL.get_or_init(|| build($lang.into(), $query)).as_ref()
            }};
        }
        match self {
            Self::Rust => cached!(tree_sitter_rust::LANGUAGE, tree_sitter_rust::TAGS_QUERY),
            Self::Python => cached!(tree_sitter_python::LANGUAGE, tree_sitter_python::TAGS_QUERY),
            Self::JavaScript => {
                cached!(
                    tree_sitter_javascript::LANGUAGE,
                    tree_sitter_javascript::TAGS_QUERY
                )
            }
            // TypeScript's tags.scm only adds TS-specific captures and inherits the
            // rest from JavaScript; concatenate both so class/function decls are seen.
            Self::TypeScript => cached!(
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
                &format!(
                    "{}\n{}",
                    tree_sitter_javascript::TAGS_QUERY,
                    tree_sitter_typescript::TAGS_QUERY
                )
            ),
            Self::Tsx => cached!(
                tree_sitter_typescript::LANGUAGE_TSX,
                &format!(
                    "{}\n{}",
                    tree_sitter_javascript::TAGS_QUERY,
                    tree_sitter_typescript::TAGS_QUERY
                )
            ),
            Self::Go => cached!(tree_sitter_go::LANGUAGE, tree_sitter_go::TAGS_QUERY),
            Self::Java => cached!(
                tree_sitter_java::LANGUAGE,
                &format!("{}\n{}", tree_sitter_java::TAGS_QUERY, JAVA_EXTRA_TAGS)
            ),
            Self::C => cached!(tree_sitter_c::LANGUAGE, tree_sitter_c::TAGS_QUERY),
            Self::Cpp => cached!(tree_sitter_cpp::LANGUAGE, tree_sitter_cpp::TAGS_QUERY),
            // The bundled c-sharp tags query fails to compile here; use ours.
            Self::CSharp => cached!(tree_sitter_c_sharp::LANGUAGE, CSHARP_TAGS),
            Self::Ruby => cached!(tree_sitter_ruby::LANGUAGE, tree_sitter_ruby::TAGS_QUERY),
            Self::Php => cached!(
                tree_sitter_php::LANGUAGE_PHP,
                &format!("{}\n{}", tree_sitter_php::TAGS_QUERY, PHP_EXTRA_TAGS)
            ),
        }
    }
}

pub(crate) fn symbols_for_path(
    source: &str,
    rel_path: &str,
) -> Result<Option<ParsedCodeFile>, String> {
    let Some(language) = Language::from_path(rel_path) else {
        return Ok(None);
    };
    let (symbols, references) = code_facts_for_language(language, source)?;
    Ok(Some(ParsedCodeFile {
        language: language.name().to_string(),
        symbols,
        references,
    }))
}

#[cfg(fuzzing)]
#[doc(hidden)]
pub fn fuzz_symbols_for_path(
    source: &str,
    rel_path: &str,
) -> Result<Option<(String, Vec<CodeSymbol>)>, String> {
    Ok(symbols_for_path(source, rel_path)?.map(|parsed| (parsed.language, parsed.symbols)))
}

fn code_facts_for_language(
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
        let line = tag.span.start.row + 1;
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
                signature,
                members,
            });
        } else {
            references.push(CodeReference {
                kind,
                name,
                line,
                import_path: None,
            });
        }
    }
    // Deterministic order for stable output and goldens.
    symbols.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.name.cmp(&b.name)));
    Ok((symbols, references))
}

/// Build a compact one-line signature for a definition tag: text from the start
/// of the definition node up to the body (a top-level `{` or `;` for brace
/// languages, `:` for Python), with all whitespace collapsed to single spaces and
/// the length capped. Falls back to the trimmed first line tree-sitter already
/// computed (`line_range`). Returns `None` if nothing useful remains.
fn signature_for(language: Language, source: &[u8], tag: &tree_sitter_tags::Tag) -> Option<String> {
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
fn truncate_chars(text: &str, max: usize) -> String {
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
fn signature_span_end(language: Language, node: &[u8]) -> Option<usize> {
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
fn members_by_definition(
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
fn field_list_node<'a>(
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

fn collect_members(
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
fn collect_ruby_members(
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

fn make_member(language: Language, field: tree_sitter::Node, source: &[u8]) -> Option<CodeMember> {
    let name = member_name(language, field, source)?;
    let signature = member_signature(field, source);
    Some(CodeMember { name, signature })
}

fn member_name(language: Language, field: tree_sitter::Node, source: &[u8]) -> Option<String> {
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

fn member_signature(field: tree_sitter::Node, source: &[u8]) -> Option<String> {
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

/// First descendant (document order) whose kind is in `kinds`.
fn first_descendant_kind<'a>(
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
    let language = Language::from_path(path)?;
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

/// A tree-sitter syntax problem in a file (used to flag broken edits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntaxIssue {
    pub line: usize,
    pub message: String,
}

/// Re-parse `source` and report tree-sitter `ERROR` / missing nodes as syntax
/// diagnostics. Empty for unsupported languages or a clean parse. Syntax-level
/// only — this is not type checking or a language server.
pub(crate) fn syntax_diagnostics(path: &str, source: &str) -> Vec<SyntaxIssue> {
    const MAX_ISSUES: usize = 20;
    let Some(language) = Language::from_path(path) else {
        return Vec::new();
    };
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
                line: node.start_position().row + 1,
                message: format!("missing {}", node.kind()),
            });
        } else if node.is_error() {
            issues.push(SyntaxIssue {
                line: node.start_position().row + 1,
                message: "syntax error".to_string(),
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
    issues.truncate(MAX_ISSUES);
    issues
}

/// One structural match from [`ast_search`]: the `@match` region's line and
/// text, plus any other captures as metavariables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AstMatch {
    pub line: usize,
    pub text: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub captures: BTreeMap<String, String>,
}

/// Whether `language_name` maps to a supported tree-sitter grammar.
pub(crate) fn ast_language_supported(language_name: &str) -> bool {
    Language::from_name(language_name).is_some()
}

/// Display language name for a path's extension, if supported.
pub(crate) fn path_language_name(path: &str) -> Option<&'static str> {
    Language::from_path(path).map(Language::name)
}

fn ast_node_text(node: tree_sitter::Node, source: &[u8]) -> String {
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
    let language = Language::from_path(path).ok_or_else(|| "unsupported language".to_string())?;
    let ts_language = language.ts_language();
    if query_src.contains("(#") {
        return Err(
            "query predicates such as (#eq? ...) are not applied yet; use a structural pattern"
                .to_string(),
        );
    }
    let query = tree_sitter::Query::new(&ts_language, query_src)
        .map_err(|err| format!("invalid query: {err}"))?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|err| format!("language error: {err}"))?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(Vec::new());
    };
    let names = query.capture_names();
    let bytes = source.as_bytes();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), bytes);
    let mut matches = Vec::new();
    while let Some(query_match) = iter.next() {
        let mut captures = BTreeMap::new();
        let mut match_node = None;
        for capture in query_match.captures {
            let name = names[capture.index as usize];
            if name == "match" {
                match_node = Some(capture.node);
            }
            captures.insert(name.to_string(), ast_node_text(capture.node, bytes));
        }
        let node = match_node.or_else(|| {
            query_match
                .captures
                .iter()
                .map(|capture| capture.node)
                .max_by_key(|node| node.byte_range().len())
        });
        let Some(node) = node else { continue };
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
    let language = Language::from_path(path).ok_or_else(|| "unsupported language".to_string())?;
    let ts_language = language.ts_language();
    if query_src.contains("(#") {
        return Err(
            "query predicates such as (#eq? ...) are not applied yet; use a structural pattern"
                .to_string(),
        );
    }
    let query = tree_sitter::Query::new(&ts_language, query_src)
        .map_err(|err| format!("invalid query: {err}"))?;
    let names = query.capture_names();
    if !names.contains(&"match") {
        return Err("rewrite query must capture the region to replace as @match".to_string());
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|err| format!("language error: {err}"))?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok((source.to_string(), 0));
    };
    let bytes = source.as_bytes();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut iter = cursor.matches(&query, tree.root_node(), bytes);
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    while let Some(query_match) = iter.next() {
        let mut values: BTreeMap<&str, &str> = BTreeMap::new();
        let mut region = None;
        for capture in query_match.captures {
            let name = names[capture.index as usize];
            let text = capture.node.utf8_text(bytes).unwrap_or_default();
            if name == "match" {
                region = Some(capture.node.byte_range());
            }
            values.insert(name, text);
        }
        let Some(range) = region else { continue };
        edits.push((range, render_template(replacement, &values)));
    }
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
    Ok((result, count))
}

fn render_template(template: &str, values: &BTreeMap<&str, &str>) -> String {
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

fn select_entries<'a>(
    snapshot: &'a CatalogSnapshot,
    paths: &[PathBuf],
) -> Vec<&'a crate::models::CatalogEntry> {
    if paths.is_empty() {
        return snapshot.entries.iter().collect();
    }

    let mut selected = BTreeSet::new();
    for path in paths {
        let raw = path.to_string_lossy().replace('\\', "/");
        let rel = raw.trim_start_matches("./").trim_end_matches('/');
        let canonical = path.canonicalize().ok();
        for (idx, entry) in snapshot.entries.iter().enumerate() {
            let rel_match = rel.is_empty()
                || entry.rel_path == rel
                || entry.rel_path.starts_with(&format!("{rel}/"));
            let abs_match = canonical
                .as_ref()
                .is_some_and(|abs| entry.abs_path == *abs || entry.abs_path.starts_with(abs));
            if rel_match || abs_match {
                selected.insert(idx);
            }
        }
    }

    selected
        .into_iter()
        .map(|idx| &snapshot.entries[idx])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str, rel_path: &str) -> ParsedCodeFile {
        symbols_for_path(source, rel_path)
            .expect("parse")
            .expect("supported language")
    }

    fn has_symbol(parsed: &ParsedCodeFile, name: &str) -> bool {
        parsed.symbols.iter().any(|symbol| symbol.name == name)
    }

    #[test]
    fn rust_definitions_and_references() {
        let parsed = parse(
            "pub struct Widget;\npub fn make_widget() -> Widget { Widget }\n",
            "lib.rs",
        );
        assert_eq!(parsed.language, "rust");
        assert!(has_symbol(&parsed, "Widget"));
        assert!(has_symbol(&parsed, "make_widget"));
    }

    #[test]
    fn python_definitions() {
        let parsed = parse(include_str!("../tests/fixtures/gamma.py"), "gamma.py");
        assert_eq!(parsed.language, "python");
        assert!(has_symbol(&parsed, "PyAlpha"));
        assert!(has_symbol(&parsed, "py_helper"));
    }

    #[test]
    fn javascript_definitions() {
        let parsed = parse(include_str!("../tests/fixtures/delta.js"), "delta.js");
        assert_eq!(parsed.language, "javascript");
        assert!(has_symbol(&parsed, "Widget"));
    }

    #[test]
    fn go_definitions_and_references() {
        let parsed = parse(include_str!("../tests/go_fixture.go"), "main.go");
        assert_eq!(parsed.language, "go");
        assert!(has_symbol(&parsed, "NewService"));
        assert!(has_symbol(&parsed, "Greet"));
        assert!(!parsed.references.is_empty());
    }

    #[test]
    fn typescript_definitions() {
        let parsed = parse(
            "export class Service {\n  greet(name: string): string { return name; }\n}\nexport function make(): Service { return new Service(); }\n",
            "svc.ts",
        );
        assert_eq!(parsed.language, "typescript");
        assert!(
            has_symbol(&parsed, "Service"),
            "symbols: {:?}",
            parsed.symbols
        );
        assert!(has_symbol(&parsed, "make"));
    }

    #[test]
    fn symbols_include_declaration_signatures() {
        let parsed = parse(
            "pub fn add(\n    left: usize,\n    right: usize,\n) -> usize {\n    left + right\n}\n",
            "math.rs",
        );
        let add = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "add")
            .expect("add symbol");
        let signature = add.signature.as_deref().unwrap_or_default();
        assert!(
            signature.starts_with("pub fn add("),
            "signature: {signature}"
        );
        assert!(signature.contains("-> usize"), "signature: {signature}");
        assert!(
            !signature.contains('{'),
            "signature must stop before the body: {signature}"
        );
    }

    #[test]
    fn struct_fields_become_members() {
        let parsed = parse(
            "pub struct Point {\n    pub x: i32,\n    y: String,\n}\n",
            "p.rs",
        );
        let point = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Point")
            .expect("Point symbol");
        let names: Vec<&str> = point.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["x", "y"]);
        assert_eq!(point.members[0].signature.as_deref(), Some("pub x: i32"));
        assert_eq!(point.members[1].signature.as_deref(), Some("y: String"));
    }

    #[test]
    fn typescript_interface_fields_become_members() {
        let parsed = parse(
            "export interface User {\n  id: number;\n  name?: string;\n}\n",
            "user.ts",
        );
        let user = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "User")
            .expect("User symbol");
        let names: Vec<&str> = user.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["id", "name"]);
        assert_eq!(user.members[1].signature.as_deref(), Some("name?: string"));
    }

    #[test]
    fn enum_variants_become_members() {
        let parsed = parse("pub enum Mode {\n    Fast,\n    Slow(u8),\n}\n", "m.rs");
        let mode = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "Mode")
            .expect("Mode symbol");
        let names: Vec<&str> = mode.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Fast", "Slow"]);
    }

    fn member_names(parsed: &ParsedCodeFile, symbol: &str) -> Vec<String> {
        parsed
            .symbols
            .iter()
            .find(|candidate| candidate.name == symbol)
            .unwrap_or_else(|| panic!("symbol {symbol} not found in {:?}", parsed.symbols))
            .members
            .iter()
            .map(|member| member.name.clone())
            .collect()
    }

    #[test]
    fn java_enum_and_record_members() {
        let parsed = parse("enum Color { RED, GREEN }\n", "Color.java");
        assert_eq!(member_names(&parsed, "Color"), ["RED", "GREEN"]);
        let parsed = parse("record Point(int x, String y) {}\n", "Point.java");
        assert_eq!(member_names(&parsed, "Point"), ["x", "y"]);
    }

    #[test]
    fn csharp_enum_and_record_members() {
        let parsed = parse("enum E { A, B }\n", "E.cs");
        assert_eq!(member_names(&parsed, "E"), ["A", "B"]);
        let parsed = parse("record R(int X, string Y);\n", "R.cs");
        assert_eq!(member_names(&parsed, "R"), ["X", "Y"]);
    }

    #[test]
    fn php_enum_and_trait_members() {
        let parsed = parse(
            "<?php\nenum Suit { case Hearts; case Spades; }\n",
            "Suit.php",
        );
        assert_eq!(member_names(&parsed, "Suit"), ["Hearts", "Spades"]);
        let parsed = parse("<?php\ntrait T { public int $a; private $b; }\n", "T.php");
        assert_eq!(member_names(&parsed, "T"), ["$a", "$b"]);
    }

    #[test]
    fn ruby_attr_accessor_members() {
        let parsed = parse(
            "class C\n  attr_accessor :a, :b\n  attr_reader :c\nend\n",
            "c.rb",
        );
        assert_eq!(member_names(&parsed, "C"), ["a", "b", "c"]);
    }

    #[test]
    fn interface_members_java_and_csharp() {
        // Interface constants/properties are members; method signatures stay
        // top-level symbols.
        let parsed = parse(
            "interface Shape {\n    int SIDES = 3;\n    double area();\n}\n",
            "Shape.java",
        );
        assert_eq!(member_names(&parsed, "Shape"), ["SIDES"]);

        let parsed = parse(
            "interface IShape {\n    int Sides { get; }\n}\n",
            "IShape.cs",
        );
        assert_eq!(member_names(&parsed, "IShape"), ["Sides"]);
    }

    #[test]
    fn ast_search_and_rewrite_rust() {
        let src = "fn a() { foo(); }\nfn b() { foo(); }\n";
        let query = "(call_expression function: (identifier) @name) @match";
        let matches = ast_search("x.rs", src, query, 10).expect("search");
        assert_eq!(matches.len(), 2);
        assert_eq!(
            matches[0].captures.get("name").map(String::as_str),
            Some("foo")
        );
        let (rewritten, count) = ast_rewrite("x.rs", src, query, "${name}_v2()").expect("rewrite");
        assert_eq!(count, 2);
        assert_eq!(rewritten, "fn a() { foo_v2(); }\nfn b() { foo_v2(); }\n");
    }

    #[test]
    fn ast_rewrite_requires_match_capture() {
        let err = ast_rewrite("x.rs", "fn a() {}\n", "(identifier) @name", "x").expect_err("needs");
        assert!(err.contains("@match"), "{err}");
    }

    #[test]
    fn ast_search_rejects_invalid_query() {
        let err =
            ast_search("x.rs", "fn a() {}\n", "(nonexistent_node) @match", 10).expect_err("bad");
        assert!(err.contains("invalid query"), "{err}");
    }

    #[test]
    fn java_definitions() {
        let parsed = parse(
            "class Greeter {\n  public String greet(String name) { return name; }\n}\n",
            "Greeter.java",
        );
        assert_eq!(parsed.language, "java");
        assert!(has_symbol(&parsed, "Greeter"));
        assert!(has_symbol(&parsed, "greet"));
    }

    #[test]
    fn ruby_definitions() {
        let parsed = parse(
            "class Greeter\n  def greet(name)\n    name\n  end\nend\n",
            "greeter.rb",
        );
        assert_eq!(parsed.language, "ruby");
        assert!(has_symbol(&parsed, "Greeter"));
        assert!(has_symbol(&parsed, "greet"));
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(
            symbols_for_path("plain text", "notes.txt")
                .expect("ok")
                .is_none()
        );
    }
}
