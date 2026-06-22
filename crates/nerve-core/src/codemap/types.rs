use super::*;

/// A symbol (definition) extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub kind: String,
    pub name: String,
    pub line: usize,
    /// 1-based byte column of the symbol name.
    #[serde(default = "default_column")]
    pub column: usize,
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
    /// 1-based byte column of the referenced name.
    #[serde(default = "default_column")]
    pub column: usize,
    /// Source language of this reference when it differs from the containing
    /// file's display language (for example, a Rust fenced block in Markdown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_path: Option<String>,
}

fn default_column() -> usize {
    1
}

impl CodeReference {
    /// Construct a reference occurrence with no embedded-language override or
    /// import target metadata.
    pub fn new(kind: impl Into<String>, name: impl Into<String>, line: usize) -> Self {
        Self {
            kind: kind.into(),
            name: name.into(),
            line,
            column: 1,
            language: None,
            import_path: None,
        }
    }

    /// Attach 1-based byte-column metadata for this reference occurrence.
    pub fn with_column(mut self, column: usize) -> Self {
        self.column = column;
        self
    }

    /// Attach source-language metadata for references extracted from embedded
    /// source snippets, such as Markdown fenced code blocks.
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Attach import-target metadata captured by language-specific tag queries.
    pub fn with_import_path(mut self, import_path: impl Into<String>) -> Self {
        self.import_path = Some(import_path.into());
        self
    }

    pub(crate) fn effective_language<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.language.as_deref().unwrap_or(fallback)
    }

    pub(crate) fn has_embedded_language(&self) -> bool {
        self.language.is_some()
    }
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
    /// Estimated tokens of this file's rendered codemap block, so callers can
    /// budget which files' structures to include without re-tokenizing.
    pub token_count: usize,
}

/// Render one file's codemap block (path header + `kind (line): signature`
/// lines, with members indented). Shared by the `get_code_structure` tool text
/// and the per-file `token_count`, so the count matches what is shown.
pub fn render_file_codemap(file: &FileCodeStructure) -> String {
    let mut out = String::new();
    out.push_str(&file.path);
    out.push('\n');
    for symbol in &file.symbols {
        match &symbol.signature {
            Some(signature) => out.push_str(&format!(
                "  {} ({}): {}\n",
                symbol.kind, symbol.line, signature
            )),
            None => out.push_str(&format!(
                "  {} {} ({})\n",
                symbol.kind, symbol.name, symbol.line
            )),
        }
        for member in &symbol.members {
            match &member.signature {
                Some(signature) => out.push_str(&format!("    - {signature}\n")),
                None => out.push_str(&format!("    - {}\n", member.name)),
            }
        }
    }
    out
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
    /// Sum of every file's `token_count`.
    pub total_tokens: usize,
}

/// Extract code structure for selected paths.
///
/// Empty `paths` means the whole catalog. Directory paths select entries by
/// prefix; file paths select exact entries. Unsupported files are omitted.
pub fn get_code_structure<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    paths: &[PathBuf],
) -> Result<CodeStructureResponse, NerveError> {
    let selected = select_entries(snapshot, paths);
    let mut files = Vec::new();
    let mut diagnostics = Vec::new();
    let mut omitted = 0usize;

    for entry in selected {
        match provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? {
            Ok(Some(parsed)) => {
                let mut file = FileCodeStructure {
                    path: entry.rel_path.clone(),
                    language: parsed.language.clone(),
                    symbols: parsed.symbols.clone(),
                    token_count: 0,
                };
                file.token_count = crate::token::count_tokens(&render_file_codemap(&file));
                files.push(file);
            }
            Ok(None) => omitted += 1,
            Err(message) => diagnostics.push(CodeStructureDiagnostic {
                path: Some(entry.rel_path.clone()),
                message,
            }),
        }
    }

    Ok(CodeStructureResponse {
        total_tokens: files.iter().map(|file| file.token_count).sum(),
        files,
        diagnostics,
        omitted,
    })
}
