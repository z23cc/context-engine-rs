//! Lightweight pure-Rust codemap extraction.
//!
//! The codemap is intentionally shallow: it reports only top-level symbols and
//! does not expand impl/class members or nested module contents.

use crate::{models::*, port::CatalogProvider, snapshot::CatalogSnapshot};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingPattern, Class, Declaration, ExportDefaultDeclarationKind, Expression, Function,
    Statement, VariableDeclaration,
};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use proc_macro2::Span as RustSpan;
use quote::ToTokens;
use ruff_python_ast::Stmt;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};
use syn::{Item, spanned::Spanned};

/// A top-level symbol extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
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

/// Extract lightweight code structure for selected paths.
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
            Ok(Some((language, symbols))) => files.push(FileCodeStructure {
                path: entry.rel_path.clone(),
                language,
                symbols: symbols.as_ref().clone(),
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
}

impl Language {
    fn from_path(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".rs") {
            return Some(Self::Rust);
        }
        if lower.ends_with(".py") || lower.ends_with(".pyi") {
            return Some(Self::Python);
        }
        if [".js", ".jsx", ".mjs", ".cjs", ".ts", ".tsx"]
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            return Some(Self::JavaScript);
        }
        None
    }

    fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
        }
    }
}

pub(crate) fn symbols_for_path(
    source: &str,
    rel_path: &str,
) -> Result<Option<(String, Vec<CodeSymbol>)>, String> {
    let Some(language) = Language::from_path(rel_path) else {
        return Ok(None);
    };
    let symbols = symbols_for_language(language, source, rel_path)?;
    Ok(Some((language.name().to_string(), symbols)))
}

#[cfg(fuzzing)]
#[doc(hidden)]
pub fn fuzz_symbols_for_path(
    source: &str,
    rel_path: &str,
) -> Result<Option<(String, Vec<CodeSymbol>)>, String> {
    symbols_for_path(source, rel_path)
}

fn symbols_for_language(
    language: Language,
    source: &str,
    rel_path: &str,
) -> Result<Vec<CodeSymbol>, String> {
    match language {
        Language::Rust => rust_symbols(source),
        Language::Python => python_symbols(source),
        Language::JavaScript => javascript_symbols(source, rel_path),
    }
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

fn rust_symbols(source: &str) -> Result<Vec<CodeSymbol>, String> {
    let file = syn::parse_file(source).map_err(|err| err.to_string())?;
    Ok(file
        .items
        .iter()
        .filter_map(item_symbol)
        .collect::<Vec<_>>())
}

fn item_symbol(item: &Item) -> Option<CodeSymbol> {
    let (kind, name, span) = match item {
        Item::Fn(item) => (
            "function",
            item.sig.ident.to_string(),
            item.sig.ident.span(),
        ),
        Item::Struct(item) => ("struct", item.ident.to_string(), item.ident.span()),
        Item::Enum(item) => ("enum", item.ident.to_string(), item.ident.span()),
        Item::Trait(item) => ("trait", item.ident.to_string(), item.ident.span()),
        Item::Impl(item) => ("impl", impl_name(item), item.impl_token.span),
        Item::Type(item) => ("type", item.ident.to_string(), item.ident.span()),
        Item::Const(item) => ("const", item.ident.to_string(), item.ident.span()),
        Item::Static(item) => ("static", item.ident.to_string(), item.ident.span()),
        Item::Mod(item) => ("mod", item.ident.to_string(), item.ident.span()),
        Item::Macro(item) => ("macro", macro_name(item)?, item.mac.path.span()),
        _ => return None,
    };

    Some(CodeSymbol {
        kind: kind.to_string(),
        name,
        line: span_line(span),
    })
}

fn impl_name(item: &syn::ItemImpl) -> String {
    let self_ty = type_name(&item.self_ty);
    if let Some((_, trait_path, _)) = &item.trait_ {
        format!("{} for {self_ty}", path_name(trait_path))
    } else {
        self_ty
    }
}

fn macro_name(item: &syn::ItemMacro) -> Option<String> {
    item.ident.as_ref().map(ToString::to_string).or_else(|| {
        item.mac
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
    })
}

fn type_name(ty: &syn::Type) -> String {
    compact_tokens(ty)
}

fn path_name(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|seg| {
            let args = if seg.arguments.is_empty() {
                String::new()
            } else {
                compact_tokens(&seg.arguments)
            };
            format!("{}{}", seg.ident, args)
        })
        .collect::<Vec<_>>()
        .join("::")
}

fn compact_tokens<T: ToTokens>(tokens: &T) -> String {
    tokens
        .to_token_stream()
        .to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" <", "<")
        .replace(" >", ">")
        .replace(" :: ", "::")
        .replace(" & ", "&")
}

fn span_line(span: RustSpan) -> usize {
    span.start().line
}

fn python_symbols(source: &str) -> Result<Vec<CodeSymbol>, String> {
    let parsed = ruff_python_parser::parse_module(source).map_err(|err| err.to_string())?;
    let line_index = LineIndex::new(source);

    Ok(parsed
        .syntax()
        .body
        .iter()
        .filter_map(|stmt| python_stmt_symbol(stmt, &line_index))
        .collect())
}

fn python_stmt_symbol(stmt: &Stmt, line_index: &LineIndex) -> Option<CodeSymbol> {
    match stmt {
        Stmt::FunctionDef(function) => Some(CodeSymbol {
            kind: "function".to_string(),
            name: function.name.id.as_str().to_string(),
            line: line_index.line(function.name.range.start().into()),
        }),
        Stmt::ClassDef(class) => Some(CodeSymbol {
            kind: "class".to_string(),
            name: class.name.id.as_str().to_string(),
            line: line_index.line(class.name.range.start().into()),
        }),
        _ => None,
    }
}

fn javascript_symbols(source: &str, rel_path: &str) -> Result<Vec<CodeSymbol>, String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(rel_path).map_err(|err| err.to_string())?;
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Err(parsed.errors.first().map_or_else(
            || "JavaScript parser panicked".to_string(),
            ToString::to_string,
        ));
    }
    if let Some(error) = parsed.errors.first() {
        return Err(error.to_string());
    }

    let line_index = LineIndex::new(source);
    let mut symbols = Vec::new();
    for statement in &parsed.program.body {
        javascript_statement_symbols(statement, &line_index, &mut symbols);
    }
    Ok(symbols)
}

fn javascript_statement_symbols(
    statement: &Statement<'_>,
    line_index: &LineIndex,
    symbols: &mut Vec<CodeSymbol>,
) {
    match statement {
        Statement::FunctionDeclaration(function) => {
            if let Some(symbol) = javascript_function_symbol(function, line_index, "function") {
                symbols.push(symbol);
            }
        }
        Statement::ClassDeclaration(class) => {
            if let Some(symbol) = javascript_class_symbol(class, line_index, "class") {
                symbols.push(symbol);
            }
        }
        Statement::VariableDeclaration(declaration) => {
            javascript_variable_symbols(declaration, line_index, symbols);
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(declaration) = &export.declaration {
                javascript_declaration_symbols(declaration, line_index, symbols);
            }
        }
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(symbol) = javascript_function_symbol(function, line_index, "function") {
                    symbols.push(symbol);
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                if let Some(symbol) = javascript_class_symbol(class, line_index, "class") {
                    symbols.push(symbol);
                }
            }
            _ => {}
        },
        _ => {}
    }
}

fn javascript_declaration_symbols(
    declaration: &Declaration<'_>,
    line_index: &LineIndex,
    symbols: &mut Vec<CodeSymbol>,
) {
    match declaration {
        Declaration::FunctionDeclaration(function) => {
            if let Some(symbol) = javascript_function_symbol(function, line_index, "function") {
                symbols.push(symbol);
            }
        }
        Declaration::ClassDeclaration(class) => {
            if let Some(symbol) = javascript_class_symbol(class, line_index, "class") {
                symbols.push(symbol);
            }
        }
        Declaration::VariableDeclaration(declaration) => {
            javascript_variable_symbols(declaration, line_index, symbols);
        }
        _ => {}
    }
}

fn javascript_function_symbol(
    function: &Function<'_>,
    line_index: &LineIndex,
    kind: &str,
) -> Option<CodeSymbol> {
    let id = function.id.as_ref()?;
    Some(CodeSymbol {
        kind: kind.to_string(),
        name: id.name.as_str().to_string(),
        line: span_start_line(id.span, line_index),
    })
}

fn javascript_class_symbol(
    class: &Class<'_>,
    line_index: &LineIndex,
    kind: &str,
) -> Option<CodeSymbol> {
    let id = class.id.as_ref()?;
    Some(CodeSymbol {
        kind: kind.to_string(),
        name: id.name.as_str().to_string(),
        line: span_start_line(id.span, line_index),
    })
}

fn javascript_variable_symbols(
    declaration: &VariableDeclaration<'_>,
    line_index: &LineIndex,
    symbols: &mut Vec<CodeSymbol>,
) {
    for declarator in &declaration.declarations {
        let Some(init) = &declarator.init else {
            continue;
        };
        if !matches!(
            init,
            Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
        ) {
            continue;
        }
        let BindingPattern::BindingIdentifier(id) = &declarator.id else {
            continue;
        };
        symbols.push(CodeSymbol {
            kind: "function".to_string(),
            name: id.name.as_str().to_string(),
            line: span_start_line(id.span, line_index),
        });
    }
}

fn span_start_line(span: Span, line_index: &LineIndex) -> usize {
    line_index.line(span.start as usize)
}

#[derive(Debug)]
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut starts = vec![0];
        for (idx, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(idx + 1);
            }
        }
        Self { starts }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|start| *start <= offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, ScanOptions};
    use std::fs;

    #[test]
    fn extracts_rust_top_level_symbols_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("lib.rs"),
            r#"
pub struct Widget;

enum Mode { Fast }

trait Render { fn render(&self); }

impl Widget {
    pub fn method(&self) {}
}

pub fn make_widget() -> Widget { Widget }
"#,
        )
        .expect("write");

        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        let response = get_code_structure(&provider, &snapshot, &[]).expect("codemap");
        let file = response.files.first().expect("file");
        let symbols: Vec<_> = file
            .symbols
            .iter()
            .map(|symbol| (symbol.kind.as_str(), symbol.name.as_str(), symbol.line))
            .collect();

        assert_eq!(
            symbols,
            vec![
                ("struct", "Widget", 2),
                ("enum", "Mode", 4),
                ("trait", "Render", 6),
                ("impl", "Widget", 8),
                ("function", "make_widget", 12),
            ]
        );
        assert!(!file.symbols.iter().any(|symbol| symbol.name == "method"));
    }

    #[test]
    fn extracts_python_top_level_symbols_only() {
        let symbols = python_symbols(include_str!("../tests/fixtures/gamma.py")).expect("parse");
        let symbols: Vec<_> = symbols
            .iter()
            .map(|symbol| (symbol.kind.as_str(), symbol.name.as_str(), symbol.line))
            .collect();

        assert_eq!(
            symbols,
            vec![
                ("class", "PyAlpha", 1),
                ("function", "py_helper", 6),
                ("function", "async_worker", 10),
            ]
        );
    }

    #[test]
    fn extracts_javascript_top_level_symbols_only() {
        let symbols = javascript_symbols(include_str!("../tests/fixtures/delta.js"), "delta.js")
            .expect("parse");
        let symbols: Vec<_> = symbols
            .iter()
            .map(|symbol| (symbol.kind.as_str(), symbol.name.as_str(), symbol.line))
            .collect();

        assert_eq!(
            symbols,
            vec![
                ("function", "jsEntry", 1),
                ("class", "Widget", 5),
                ("function", "runTask", 9),
                ("function", "exportedArrow", 10),
                ("function", "makeThing", 11),
                ("class", "ExportedThing", 17),
            ]
        );
    }

    #[test]
    fn names_trait_impls_as_trait_for_type() {
        let symbols = rust_symbols("impl CatalogProvider for FsCatalogProvider {}").expect("parse");
        assert_eq!(symbols[0].kind, "impl");
        assert_eq!(symbols[0].name, "CatalogProvider for FsCatalogProvider");
    }
}
