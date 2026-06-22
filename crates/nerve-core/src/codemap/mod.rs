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
mod ast;
mod block;
mod imports;
mod language;
mod markdown;
mod selection;
mod summarize;
mod summary_cache;
mod symbols;
mod types;

pub use ast::AstMatch;
pub use block::SyntaxIssue;
pub use types::{
    CodeMember, CodeReference, CodeStructureDiagnostic, CodeStructureResponse, CodeSymbol,
    FileCodeStructure, ParsedCodeFile, get_code_structure, render_file_codemap,
};

pub(crate) use ast::{
    ast_language_supported, ast_rewrite, ast_rewrite_pattern, ast_search, ast_search_language,
    ast_search_pattern, ast_search_pattern_language, path_language_name,
};
pub(crate) use block::{ContainingBlockError, block_span, containing_block_span};
pub(crate) use summarize::{render_summary, summarize_source};
#[cfg(fuzzing)]
pub use symbols::fuzz_symbols_for_path;
pub(crate) use symbols::symbols_for_path;

use language::*;
use selection::*;

pub(crate) fn path_supports_codemap(path: &str) -> bool {
    Language::from_path(path).is_some() || markdown::is_markdown_path(path)
}

pub(crate) fn source_language_name(path: &str, source: &str) -> Option<&'static str> {
    Language::from_path_or_source(path, source).map(Language::name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbeddedSource {
    pub(crate) language: &'static str,
    pub(crate) start_line_offset: usize,
    pub(crate) source: String,
}

pub(crate) fn path_supports_embedded_sources(path: &str) -> bool {
    markdown::is_markdown_path(path)
}

pub(crate) fn embedded_sources(path: &str, source: &str) -> Vec<EmbeddedSource> {
    if !markdown::is_markdown_path(path) {
        return Vec::new();
    }
    let lines = split_line_segments(source);
    markdown::supported_fences(&lines)
        .into_iter()
        .map(|fence| EmbeddedSource {
            language: fence.language.name(),
            start_line_offset: fence.body.start,
            source: markdown::fence_source(&lines, &fence),
        })
        .collect()
}

pub(crate) fn syntax_diagnostics(path: &str, source: &str) -> Vec<SyntaxIssue> {
    if !markdown::is_markdown_path(path) {
        return block::syntax_diagnostics(path, source);
    }
    let lines = split_line_segments(source);
    let mut issues = Vec::new();
    for fence in markdown::supported_fences(&lines) {
        let local_source = markdown::fence_source(&lines, &fence);
        issues.extend(block::syntax_diagnostics_for_language(
            fence.language,
            &local_source,
            fence.body.start,
            Some(&format!("{} fenced code", fence.language.name())),
        ));
        if issues.len() >= block::MAX_SYNTAX_ISSUES {
            break;
        }
    }
    issues.sort_by_key(|issue| issue.line);
    issues.dedup();
    issues.truncate(block::MAX_SYNTAX_ISSUES);
    issues
}

pub(crate) fn embedded_block_span(
    path: &str,
    source: &str,
    first_line: usize,
    last_line: usize,
) -> Result<Option<(usize, usize)>, ContainingBlockError> {
    if !markdown::is_markdown_path(path) {
        return Ok(None);
    }
    let lines = split_line_segments(source);
    for fence in markdown::supported_fences(&lines) {
        if !range_is_inside_fence(first_line, last_line, &fence.body) {
            continue;
        }
        let local_source = markdown::fence_source(&lines, &fence);
        let local_first = first_line - fence.body.start;
        let local_last = last_line - fence.body.start;
        if let Some(span) =
            block::block_span_for_language(fence.language, &local_source, local_first)
            && span.1 > span.0
        {
            return Ok(Some(host_span(span, fence.body.start)));
        }
        return block::containing_block_span_for_language(
            fence.language,
            &local_source,
            local_first,
            local_last,
        )
        .map(|span| span.map(|span| host_span(span, fence.body.start)));
    }
    Ok(None)
}

fn range_is_inside_fence(
    first_line: usize,
    last_line: usize,
    body: &std::ops::Range<usize>,
) -> bool {
    let host_first = body.start + 1;
    let host_last = body.end;
    first_line >= host_first && last_line <= host_last
}

fn host_span((first, last): (usize, usize), offset: usize) -> (usize, usize) {
    (first + offset, last + offset)
}

fn split_line_segments(source: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = source.split_inclusive('\n').collect();
    if lines.is_empty() && !source.is_empty() {
        lines.push(source);
    }
    lines
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
        let parsed = parse(include_str!("../../tests/fixtures/gamma.py"), "gamma.py");
        assert_eq!(parsed.language, "python");
        assert!(has_symbol(&parsed, "PyAlpha"));
        assert!(has_symbol(&parsed, "py_helper"));
    }

    #[test]
    fn javascript_definitions() {
        let parsed = parse(include_str!("../../tests/fixtures/delta.js"), "delta.js");
        assert_eq!(parsed.language, "javascript");
        assert!(has_symbol(&parsed, "Widget"));
    }

    #[test]
    fn go_definitions_and_references() {
        let parsed = parse(include_str!("../../tests/go_fixture.go"), "main.go");
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
    fn ast_search_and_rewrite_pattern_rust() {
        let src = "fn main() { foo(one); bar(one); foo(two); foo(one, two); }\n";
        let matches = ast_search_pattern("x.rs", src, "foo($ARG)", 10).expect("search");
        assert_eq!(matches.len(), 2);
        assert_eq!(
            matches[0].captures.get("ARG").map(String::as_str),
            Some("one")
        );
        assert_eq!(
            matches[1].captures.get("ARG").map(String::as_str),
            Some("two")
        );

        let (rewritten, count) =
            ast_rewrite_pattern("x.rs", src, "foo($ARG)", "baz(${ARG})").expect("rewrite");
        assert_eq!(count, 2);
        assert_eq!(
            rewritten,
            "fn main() { baz(one); bar(one); baz(two); foo(one, two); }\n"
        );
    }

    #[test]
    fn ast_rewrite_preserves_full_multiline_capture() {
        let src = "fn a() {\n    foo();\n}\n";
        let query = "(function_item body: (block) @body) @match";
        let (rewritten, count) =
            ast_rewrite("x.rs", src, query, "fn b() ${body}").expect("rewrite");
        assert_eq!(count, 1);
        assert_eq!(rewritten, "fn b() {\n    foo();\n}\n");
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
    fn reference_columns_are_one_based_byte_columns() {
        let parsed = parse(
            "pub fn helper() {}\n\npub fn caller() { let café = 1; helper(); }\n",
            "lib.rs",
        );
        let helper_ref = parsed
            .references
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("helper reference");

        assert_eq!(helper_ref.line, 3);
        assert_eq!(helper_ref.column, 34);
    }

    #[test]
    fn shebang_python_without_extension_is_supported() {
        let parsed = parse(
            "#!/usr/bin/env python3\nclass ScriptThing:\n    pass\n\ndef main():\n    return ScriptThing()\n",
            "script",
        );
        assert_eq!(parsed.language, "python");
        assert!(has_symbol(&parsed, "ScriptThing"));
        assert!(has_symbol(&parsed, "main"));
    }

    #[test]
    fn shebang_language_detection_covers_supported_script_interpreters() {
        for (source, expected) in [
            (
                "#!/usr/bin/env node\nfunction main() {}\n",
                Some("javascript"),
            ),
            (
                "#!/usr/bin/env deno\nfunction main(): void {}\n",
                Some("typescript"),
            ),
            ("#!/usr/bin/env ruby\ndef main\nend\n", Some("ruby")),
            (
                "#!/usr/bin/env php\n<?php function main() {}\n",
                Some("php"),
            ),
            ("#!/usr/bin/env bash -c 'python3 tool'\n", None),
        ] {
            assert_eq!(source_language_name("script", source), expected, "{source}");
        }
    }

    #[test]
    fn markdown_rust_fence_extracts_symbols_on_host_lines() {
        let parsed = parse(
            "# Docs\n\n```rust\npub fn documented() {}\n```\n",
            "README.md",
        );
        assert_eq!(parsed.language, "markdown");
        let symbol = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "documented")
            .expect("documented symbol");
        assert_eq!(symbol.line, 4);
        assert_eq!(symbol.signature.as_deref(), Some("pub fn documented()"));
    }

    #[test]
    fn markdown_fence_aliases_and_unsupported_fences_are_deterministic() {
        let parsed = parse(
            "```text\nfn ignored() {}\n```\n\n```rs\npub struct Alias;\n```\n",
            "notes.markdown",
        );
        assert_eq!(parsed.language, "markdown");
        assert!(has_symbol(&parsed, "Alias"));
        assert!(!has_symbol(&parsed, "ignored"));
    }

    #[test]
    fn markdown_fences_follow_commonmark_indentation() {
        assert!(
            symbols_for_path("    ```rust\n    fn ignored() {}\n    ```\n", "notes.md")
                .expect("ok")
                .is_none()
        );
        let parsed = parse(
            "   ```rust\n   pub fn accepted() { helper(); }\n   pub fn helper() {}\n   ```\n",
            "notes.md",
        );
        let accepted = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "accepted")
            .expect("accepted");
        let helper_ref = parsed
            .references
            .iter()
            .find(|reference| reference.name == "helper")
            .expect("helper reference");
        assert_eq!(accepted.column, 11);
        assert_eq!(helper_ref.column, 24);
    }

    #[test]
    fn markdown_embedded_references_carry_source_language() {
        let parsed = parse(
            "```python\nclass Alias:\n    pass\n\ndef make():\n    return Alias()\n```\n",
            "notes.md",
        );
        assert!(has_symbol(&parsed, "Alias"));
        assert!(has_symbol(&parsed, "make"));
        let alias_ref = parsed
            .references
            .iter()
            .find(|reference| reference.name == "Alias")
            .expect("Alias reference");
        assert_eq!(alias_ref.language.as_deref(), Some("python"));
        assert_eq!(alias_ref.line, 6);
    }

    #[test]
    fn markdown_reference_only_fence_is_retained() {
        let parsed = parse("```python\nAlias()\n```\n", "notes.md");
        assert!(parsed.symbols.is_empty());
        let alias_ref = parsed
            .references
            .iter()
            .find(|reference| reference.name == "Alias")
            .expect("Alias reference");
        assert_eq!(alias_ref.language.as_deref(), Some("python"));
        assert_eq!(alias_ref.line, 2);
    }

    #[test]
    fn markdown_unterminated_supported_fence_is_parsed_to_eof() {
        let parsed = parse("intro\n```python\ndef loose():\n    return 1\n", "notes.md");
        let symbol = parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == "loose")
            .expect("loose symbol");
        assert_eq!(symbol.line, 3);
    }

    #[test]
    fn markdown_fenced_code_syntax_diagnostics_use_host_lines() {
        let issues = syntax_diagnostics(
            "notes.md",
            "# Notes\n\n```rust\npub fn broken() {\n    let = 1;\n}\n```\n",
        );

        assert!(
            issues
                .iter()
                .any(|issue| issue.line == 5 && issue.message.starts_with("rust fenced code: ")),
            "issues: {issues:?}"
        );
    }

    #[test]
    fn markdown_without_supported_fences_returns_none() {
        assert!(
            symbols_for_path("```text\nfn ignored() {}\n```\n", "notes.md")
                .expect("ok")
                .is_none()
        );
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
