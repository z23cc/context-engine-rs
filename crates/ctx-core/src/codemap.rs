//! Lightweight pure-Rust codemap extraction.
//!
//! The codemap is intentionally shallow: it reports only top-level symbols and
//! does not expand impl/class members or nested module contents.

use crate::{models::*, port::CatalogProvider, snapshot::CatalogSnapshot};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, Class, Declaration, ExportDefaultDeclarationKind, Expression,
    Function, ImportDeclarationSpecifier, Statement, VariableDeclaration,
};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};
use proc_macro2::Span as RustSpan;
use quote::ToTokens;
use ruff_python_ast::{Expr, Stmt, visitor};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};
use syn::{Item, spanned::Spanned, visit};

/// A top-level symbol extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
}

/// An AST-derived reference occurrence used internally by repo-map.
///
/// These references are node-level names from imports, calls, attributes,
/// identifiers, and type paths. They intentionally do not perform full scope,
/// type, alias, re-export, or multi-definition resolution; repo-map resolves
/// them later by same-language name matching against top-level codemap symbols.
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
) -> Result<Option<ParsedCodeFile>, String> {
    let Some(language) = Language::from_path(rel_path) else {
        return Ok(None);
    };
    let (symbols, references) = code_facts_for_language(language, source, rel_path)?;
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
    symbols_for_path(source, rel_path)
}

fn code_facts_for_language(
    language: Language,
    source: &str,
    rel_path: &str,
) -> Result<(Vec<CodeSymbol>, Vec<CodeReference>), String> {
    match language {
        Language::Rust => rust_code_facts(source),
        Language::Python => python_code_facts(source),
        Language::JavaScript => javascript_code_facts(source, rel_path),
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

fn rust_code_facts(source: &str) -> Result<(Vec<CodeSymbol>, Vec<CodeReference>), String> {
    let file = syn::parse_file(source).map_err(|err| err.to_string())?;
    let symbols = file
        .items
        .iter()
        .filter_map(item_symbol)
        .collect::<Vec<_>>();
    let mut collector = RustReferenceCollector::default();
    visit::visit_file(&mut collector, &file);
    Ok((symbols, collector.references))
}

#[cfg(test)]
fn rust_symbols(source: &str) -> Result<Vec<CodeSymbol>, String> {
    rust_code_facts(source).map(|(symbols, _)| symbols)
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

fn python_code_facts(source: &str) -> Result<(Vec<CodeSymbol>, Vec<CodeReference>), String> {
    let parsed = ruff_python_parser::parse_module(source).map_err(|err| err.to_string())?;
    let line_index = LineIndex::new(source);
    let body = &parsed.syntax().body;
    let symbols = body
        .iter()
        .filter_map(|stmt| python_stmt_symbol(stmt, &line_index))
        .collect();
    let mut collector = PythonReferenceCollector {
        line_index: &line_index,
        references: Vec::new(),
    };
    visitor::walk_body(&mut collector, body);
    Ok((symbols, collector.references))
}

#[cfg(test)]
fn python_symbols(source: &str) -> Result<Vec<CodeSymbol>, String> {
    python_code_facts(source).map(|(symbols, _)| symbols)
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

fn javascript_code_facts(
    source: &str,
    rel_path: &str,
) -> Result<(Vec<CodeSymbol>, Vec<CodeReference>), String> {
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
    let mut references = Vec::new();
    for statement in &parsed.program.body {
        javascript_statement_symbols(statement, &line_index, &mut symbols);
        javascript_statement_references(statement, &line_index, &mut references);
    }
    Ok((symbols, references))
}

#[cfg(test)]
fn javascript_symbols(source: &str, rel_path: &str) -> Result<Vec<CodeSymbol>, String> {
    javascript_code_facts(source, rel_path).map(|(symbols, _)| symbols)
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

fn code_reference(kind: &str, name: impl Into<String>, line: usize) -> CodeReference {
    CodeReference {
        kind: kind.to_string(),
        name: name.into(),
        line,
        import_path: None,
    }
}

fn import_reference(
    name: impl Into<String>,
    line: usize,
    import_path: impl Into<Option<String>>,
) -> CodeReference {
    CodeReference {
        kind: "import".to_string(),
        name: name.into(),
        line,
        import_path: import_path.into(),
    }
}

#[derive(Default)]
struct RustReferenceCollector {
    references: Vec<CodeReference>,
}

impl RustReferenceCollector {
    fn push_path_reference(&mut self, kind: &str, path: &syn::Path, span: RustSpan) {
        if let Some(name) = path.segments.last().map(|seg| seg.ident.to_string()) {
            self.references
                .push(code_reference(kind, name, span_line(span)));
        }
    }

    fn push_use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.push_use_tree(&path.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(name) => {
                let symbol = name.ident.to_string();
                let mut full_path = prefix.clone();
                full_path.push(symbol.clone());
                self.references.push(import_reference(
                    symbol,
                    span_line(name.ident.span()),
                    Some(full_path.join("::")),
                ));
            }
            syn::UseTree::Rename(rename) => {
                let symbol = rename.ident.to_string();
                let mut full_path = prefix.clone();
                full_path.push(symbol.clone());
                self.references.push(import_reference(
                    symbol,
                    span_line(rename.ident.span()),
                    Some(full_path.join("::")),
                ));
            }
            syn::UseTree::Glob(glob) => {
                if let Some(module) = prefix.last() {
                    self.references.push(import_reference(
                        module.clone(),
                        span_line(glob.star_token.span),
                        Some(prefix.join("::")),
                    ));
                }
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.push_use_tree(item, prefix);
                }
            }
        }
    }
}

impl<'ast> visit::Visit<'ast> for RustReferenceCollector {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        let mut prefix = Vec::new();
        self.push_use_tree(&item.tree, &mut prefix);
    }

    fn visit_expr_call(&mut self, expr: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = expr.func.as_ref() {
            self.push_path_reference("call", &path.path, path.path.span());
        }
        for arg in &expr.args {
            visit::visit_expr(self, arg);
        }
    }

    fn visit_expr_method_call(&mut self, expr: &'ast syn::ExprMethodCall) {
        self.references.push(code_reference(
            "method_call",
            expr.method.to_string(),
            span_line(expr.method.span()),
        ));
        visit::visit_expr(self, &expr.receiver);
        for arg in &expr.args {
            visit::visit_expr(self, arg);
        }
    }

    fn visit_type_path(&mut self, ty: &'ast syn::TypePath) {
        self.push_path_reference("type", &ty.path, ty.path.span());
        visit::visit_type_path(self, ty);
    }
}

struct PythonReferenceCollector<'a> {
    line_index: &'a LineIndex,
    references: Vec<CodeReference>,
}

impl PythonReferenceCollector<'_> {
    fn line_for_range(&self, range: ruff_text_size::TextRange) -> usize {
        self.line_index.line(range.start().into())
    }
}

impl<'a> visitor::Visitor<'a> for PythonReferenceCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let full = alias.name.id.as_str().to_string();
                    let symbol = full.rsplit('.').next().unwrap_or(&full).to_string();
                    self.references.push(import_reference(
                        symbol,
                        self.line_for_range(alias.range),
                        Some(full),
                    ));
                }
            }
            Stmt::ImportFrom(import) => {
                let module = import
                    .module
                    .as_ref()
                    .map(|module| module.id.as_str().to_string());
                for alias in &import.names {
                    let symbol = alias.name.id.as_str().to_string();
                    let path = module
                        .as_ref()
                        .map(|module| format!("{module}.{symbol}"))
                        .or_else(|| Some(symbol.clone()));
                    self.references.push(import_reference(
                        symbol,
                        self.line_for_range(alias.range),
                        path,
                    ));
                }
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Call(call) => {
                if let Some(name) = python_expr_name(&call.func) {
                    self.references.push(code_reference(
                        "call",
                        name,
                        self.line_for_range(call.range),
                    ));
                }
                visitor::walk_expr(self, expr);
            }
            Expr::Name(name) => {
                self.references.push(code_reference(
                    "identifier",
                    name.id.as_str(),
                    self.line_for_range(name.range),
                ));
            }
            Expr::Attribute(attribute) => {
                self.references.push(code_reference(
                    "attribute",
                    attribute.attr.id.as_str(),
                    self.line_for_range(attribute.attr.range),
                ));
                visitor::walk_expr(self, expr);
            }
            _ => visitor::walk_expr(self, expr),
        }
    }
}

fn python_expr_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str().to_string()),
        Expr::Attribute(attribute) => Some(attribute.attr.id.as_str().to_string()),
        _ => None,
    }
}

fn javascript_statement_references(
    statement: &Statement<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    match statement {
        Statement::ImportDeclaration(import) => {
            javascript_import_references(import, line_index, references)
        }
        Statement::ExpressionStatement(statement) => {
            javascript_expression_references(&statement.expression, line_index, references);
        }
        Statement::ReturnStatement(statement) => {
            if let Some(argument) = &statement.argument {
                javascript_expression_references(argument, line_index, references);
            }
        }
        Statement::VariableDeclaration(declaration) => {
            javascript_variable_references(declaration, line_index, references);
        }
        Statement::FunctionDeclaration(function) => {
            javascript_function_body_references(function, line_index, references);
        }
        Statement::ExportNamedDeclaration(export) => {
            if let Some(declaration) = &export.declaration {
                javascript_declaration_references(declaration, line_index, references);
            }
        }
        Statement::ExportDefaultDeclaration(export) => match &export.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                javascript_function_body_references(function, line_index, references);
            }
            ExportDefaultDeclarationKind::ClassDeclaration(_) => {}
            _ => {
                if let Some(expression) = export.declaration.as_expression() {
                    javascript_expression_references(expression, line_index, references);
                }
            }
        },
        _ => {}
    }
}

fn javascript_declaration_references(
    declaration: &Declaration<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    match declaration {
        Declaration::VariableDeclaration(declaration) => {
            javascript_variable_references(declaration, line_index, references);
        }
        Declaration::FunctionDeclaration(function) => {
            javascript_function_body_references(function, line_index, references);
        }
        _ => {}
    }
}

fn javascript_import_references(
    import: &oxc_ast::ast::ImportDeclaration<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    let source = import.source.value.as_str().to_string();
    if let Some(specifiers) = &import.specifiers {
        for specifier in specifiers {
            let (name, span) = match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    (module_export_name(&specifier.imported), specifier.span)
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    (specifier.local.name.as_str().to_string(), specifier.span)
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    (specifier.local.name.as_str().to_string(), specifier.span)
                }
            };
            references.push(import_reference(
                name,
                span_start_line(span, line_index),
                Some(source.clone()),
            ));
        }
    } else {
        references.push(import_reference(
            source.clone(),
            span_start_line(import.source.span, line_index),
            Some(source),
        ));
    }
}

fn module_export_name(name: &oxc_ast::ast::ModuleExportName<'_>) -> String {
    match name {
        oxc_ast::ast::ModuleExportName::IdentifierName(name) => name.name.as_str().to_string(),
        oxc_ast::ast::ModuleExportName::IdentifierReference(name) => name.name.as_str().to_string(),
        oxc_ast::ast::ModuleExportName::StringLiteral(name) => name.value.as_str().to_string(),
    }
}

fn javascript_variable_references(
    declaration: &VariableDeclaration<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    for declarator in &declaration.declarations {
        if let Some(init) = &declarator.init {
            javascript_expression_references(init, line_index, references);
        }
    }
}

fn javascript_function_body_references(
    function: &Function<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    if let Some(body) = &function.body {
        for statement in &body.statements {
            javascript_statement_references(statement, line_index, references);
        }
    }
}

fn javascript_expression_references(
    expression: &Expression<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    match expression {
        Expression::Identifier(identifier) => references.push(code_reference(
            "identifier",
            identifier.name.as_str(),
            span_start_line(identifier.span, line_index),
        )),
        Expression::CallExpression(call) => {
            if let Some(name) = javascript_expression_name(&call.callee) {
                if name == "require" {
                    if let Some(source) = javascript_require_source(call) {
                        references.push(import_reference(
                            source.clone(),
                            span_start_line(call.span, line_index),
                            Some(source),
                        ));
                    }
                } else {
                    references.push(code_reference(
                        "call",
                        name,
                        span_start_line(call.span, line_index),
                    ));
                }
            }
            javascript_expression_references(&call.callee, line_index, references);
            for argument in &call.arguments {
                javascript_argument_references(argument, line_index, references);
            }
        }
        Expression::StaticMemberExpression(member) => {
            references.push(code_reference(
                "attribute",
                member.property.name.as_str(),
                span_start_line(member.property.span, line_index),
            ));
            javascript_expression_references(&member.object, line_index, references);
        }
        Expression::ComputedMemberExpression(member) => {
            javascript_expression_references(&member.object, line_index, references);
            javascript_expression_references(&member.expression, line_index, references);
        }
        Expression::BinaryExpression(binary) => {
            javascript_expression_references(&binary.left, line_index, references);
            javascript_expression_references(&binary.right, line_index, references);
        }
        Expression::LogicalExpression(logical) => {
            javascript_expression_references(&logical.left, line_index, references);
            javascript_expression_references(&logical.right, line_index, references);
        }
        Expression::AssignmentExpression(assignment) => {
            javascript_assignment_target_references(&assignment.left, line_index, references);
            javascript_expression_references(&assignment.right, line_index, references);
        }
        Expression::ArrowFunctionExpression(function) => {
            for statement in &function.body.statements {
                javascript_statement_references(statement, line_index, references);
            }
        }
        Expression::FunctionExpression(function) => {
            javascript_function_body_references(function, line_index, references);
        }
        Expression::ParenthesizedExpression(parenthesized) => {
            javascript_expression_references(&parenthesized.expression, line_index, references);
        }
        Expression::ConditionalExpression(conditional) => {
            javascript_expression_references(&conditional.test, line_index, references);
            javascript_expression_references(&conditional.consequent, line_index, references);
            javascript_expression_references(&conditional.alternate, line_index, references);
        }
        Expression::SequenceExpression(sequence) => {
            for expr in &sequence.expressions {
                javascript_expression_references(expr, line_index, references);
            }
        }
        Expression::ArrayExpression(array) => {
            for element in &array.elements {
                if let Some(expression) = element.as_expression() {
                    javascript_expression_references(expression, line_index, references);
                }
            }
        }
        _ => {}
    }
}

fn javascript_require_source(call: &oxc_ast::ast::CallExpression<'_>) -> Option<String> {
    let first = call.arguments.first()?;
    let expression = first.as_expression()?;
    if let Expression::StringLiteral(literal) = expression {
        return Some(literal.value.as_str().to_string());
    }
    None
}

fn javascript_argument_references(
    argument: &Argument<'_>,
    line_index: &LineIndex,
    references: &mut Vec<CodeReference>,
) {
    match argument {
        Argument::SpreadElement(spread) => {
            javascript_expression_references(&spread.argument, line_index, references);
        }
        _ => javascript_expression_references(argument.to_expression(), line_index, references),
    }
}

fn javascript_assignment_target_references(
    _target: &oxc_ast::ast::AssignmentTarget<'_>,
    _line_index: &LineIndex,
    _references: &mut Vec<CodeReference>,
) {
}

fn javascript_expression_name(expression: &Expression<'_>) -> Option<String> {
    match expression {
        Expression::Identifier(identifier) => Some(identifier.name.as_str().to_string()),
        Expression::StaticMemberExpression(member) => {
            Some(member.property.name.as_str().to_string())
        }
        _ => None,
    }
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
