use super::*;

#[derive(serde::Serialize)]
pub(super) struct AstFileMatch {
    pub(super) path: String,
    pub(super) line: usize,
    pub(super) text: String,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub(super) captures: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
pub(super) struct AstSearchResponse {
    pub(super) matches: Vec<AstFileMatch>,
    pub(super) files_scanned: usize,
}

impl ToolText for AstSearchResponse {
    fn tool_text(&self) -> String {
        if self.matches.is_empty() {
            return format!("(no matches in {} files)\n", self.files_scanned);
        }
        let mut out = String::new();
        for item in &self.matches {
            out.push_str(&format!("{}:{}  {}\n", item.path, item.line, item.text));
        }
        out
    }
}

/// True if `rel_path` is `scope` itself or lives under directory `scope`.
pub(super) fn path_in_scope(rel_path: &str, scope: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    scope.is_empty() || rel_path == scope || rel_path.starts_with(&format!("{scope}/"))
}

pub(super) fn ensure_ast_language(language: &str, mode: &'static str) -> Result<(), DispatchError> {
    if crate::codemap::ast_language_supported(language) {
        return Ok(());
    }
    Err(DispatchError::Edit(edit::EditError::Parse {
        mode,
        detail: format!("unsupported language: {language}"),
    }))
}

pub(super) fn ast_search_response<P>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    args: &AstSearchArgs,
    cancel: &CancelToken,
) -> Result<AstSearchResponse, DispatchError>
where
    P: DispatchProvider,
{
    let mut matches = Vec::new();
    let mut files_scanned = 0usize;
    for entry in &snapshot.entries {
        if !ast_entry_matches_scope(entry, args) {
            continue;
        }
        let Ok(bytes) = provider.read_bytes(&entry.abs_path) else {
            continue;
        };
        let source = String::from_utf8_lossy(&bytes);
        let source_language = crate::codemap::source_language_name(&entry.rel_path, &source);
        if source_language == Some(args.language.as_str()) {
            files_scanned += 1;
            push_ast_matches(&mut matches, entry, &source, args)?;
        } else {
            let scanned = push_embedded_ast_matches(&mut matches, entry, &source, args)?;
            files_scanned += usize::from(scanned);
        }
        if matches.len() >= args.max_results {
            break;
        }
        cancel.check_cancelled()?;
    }
    Ok(AstSearchResponse {
        matches,
        files_scanned,
    })
}

pub(super) fn ast_rewrite(
    args: &AstEditArgs,
    source: &str,
) -> Result<(String, usize), DispatchError> {
    let input = ast_input(&args.query, &args.pattern, args.mode.as_deref(), "ast_edit")?;
    let result = match input {
        AstInput::Query(query) => {
            crate::codemap::ast_rewrite(&args.path, source, query, &args.replacement)
        }
        AstInput::Pattern(pattern) => {
            crate::codemap::ast_rewrite_pattern(&args.path, source, pattern, &args.replacement)
        }
    };
    result.map_err(|detail| {
        DispatchError::Edit(edit::EditError::Parse {
            mode: "ast_edit",
            detail,
        })
    })
}

fn ast_entry_matches_scope(entry: &crate::CatalogEntry, args: &AstSearchArgs) -> bool {
    let path_language = crate::codemap::path_language_name(&entry.rel_path);
    let in_scope = args.paths.is_empty()
        || args
            .paths
            .iter()
            .any(|scope| path_in_scope(&entry.rel_path, scope));
    if !in_scope {
        return false;
    }
    match path_language {
        Some(language) => language == args.language,
        None => {
            crate::codemap::path_supports_embedded_sources(&entry.rel_path)
                || extensionless_path(&entry.rel_path)
                || args
                    .paths
                    .iter()
                    .any(|scope| scope.trim_end_matches('/') == entry.rel_path)
        }
    }
}

fn extensionless_path(path: &str) -> bool {
    std::path::Path::new(path).extension().is_none()
}

#[derive(Clone, Copy)]
enum AstInput<'a> {
    Query(&'a str),
    Pattern(&'a str),
}

fn ast_input<'a>(
    query: &'a Option<String>,
    pattern: &'a Option<String>,
    mode: Option<&str>,
    tool: &'static str,
) -> Result<AstInput<'a>, DispatchError> {
    match (mode, query.as_deref(), pattern.as_deref()) {
        (Some("query"), Some(query), _) | (None, Some(query), _) => Ok(AstInput::Query(query)),
        (Some("pattern"), _, Some(pattern)) | (None, None, Some(pattern)) => {
            Ok(AstInput::Pattern(pattern))
        }
        (Some("query"), None, _) => ast_input_error(tool, "mode `query` requires `query`"),
        (Some("pattern"), _, None) => ast_input_error(tool, "mode `pattern` requires `pattern`"),
        (Some(other), _, _) => ast_input_error(tool, &format!("unknown AST mode: {other}")),
        (None, None, None) => ast_input_error(tool, "provide either `query` or `pattern`"),
    }
}

fn ast_input_error<T>(tool: &'static str, detail: &str) -> Result<T, DispatchError> {
    Err(DispatchError::Edit(edit::EditError::Parse {
        mode: tool,
        detail: detail.to_string(),
    }))
}

fn push_ast_matches(
    matches: &mut Vec<AstFileMatch>,
    entry: &crate::CatalogEntry,
    source: &str,
    args: &AstSearchArgs,
) -> Result<(), DispatchError> {
    let remaining = args.max_results.saturating_sub(matches.len());
    if remaining == 0 {
        return Ok(());
    }
    let input = ast_input(
        &args.query,
        &args.pattern,
        args.mode.as_deref(),
        "ast_search",
    )?;
    let found = ast_matches_for_input(input, &entry.rel_path, source, &args.language, remaining)?;
    push_ast_items(matches, &entry.rel_path, found, 0, args.max_results);
    Ok(())
}

fn push_embedded_ast_matches(
    matches: &mut Vec<AstFileMatch>,
    entry: &crate::CatalogEntry,
    source: &str,
    args: &AstSearchArgs,
) -> Result<bool, DispatchError> {
    let input = ast_input(
        &args.query,
        &args.pattern,
        args.mode.as_deref(),
        "ast_search",
    )?;
    let mut scanned = false;
    for embedded in crate::codemap::embedded_sources(&entry.rel_path, source) {
        if embedded.language != args.language {
            continue;
        }
        scanned = true;
        let remaining = args.max_results.saturating_sub(matches.len());
        if remaining == 0 {
            break;
        }
        let found = ast_matches_for_input(
            input,
            &entry.rel_path,
            &embedded.source,
            embedded.language,
            remaining,
        )?;
        push_ast_items(
            matches,
            &entry.rel_path,
            found,
            embedded.start_line_offset,
            args.max_results,
        );
    }
    Ok(scanned)
}

fn ast_matches_for_input(
    input: AstInput<'_>,
    path: &str,
    source: &str,
    language: &str,
    max: usize,
) -> Result<Vec<crate::codemap::AstMatch>, DispatchError> {
    let native_language = crate::codemap::source_language_name(path, source) == Some(language);
    let found = match input {
        AstInput::Query(query) => {
            if native_language {
                crate::codemap::ast_search(path, source, query, max)
            } else {
                crate::codemap::ast_search_language(language, source, query, max)
            }
        }
        AstInput::Pattern(pattern) => {
            if native_language {
                crate::codemap::ast_search_pattern(path, source, pattern, max)
            } else {
                crate::codemap::ast_search_pattern_language(language, source, pattern, max)
            }
        }
    };
    found.map_err(|detail| {
        DispatchError::Edit(edit::EditError::Parse {
            mode: "ast_search",
            detail,
        })
    })
}

fn push_ast_items(
    matches: &mut Vec<AstFileMatch>,
    path: &str,
    found: Vec<crate::codemap::AstMatch>,
    line_offset: usize,
    max_results: usize,
) {
    for item in found {
        matches.push(AstFileMatch {
            path: path.to_string(),
            line: item.line + line_offset,
            text: item.text,
            captures: item.captures,
        });
        if matches.len() >= max_results {
            break;
        }
    }
}
