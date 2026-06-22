use super::*;

#[derive(Default)]
pub(super) struct ImportRelationship {
    pub(super) imports: bool,
    pub(super) aliased: bool,
}

pub(super) fn file_import_relationship<P: CatalogProvider + ?Sized>(
    provider: &P,
    files: &[crate::repomap::IndexedFile],
    file_index: usize,
    definition_index: usize,
    symbol: &str,
) -> Result<ImportRelationship, NerveError> {
    let mut relationship = ImportRelationship::default();
    let mut source = None;
    for reference in &files[file_index].references {
        if reference.kind != "import" || reference.name != symbol {
            continue;
        }
        if crate::repomap::resolve_import_reference(files, file_index, reference)
            != Some(definition_index)
        {
            continue;
        }
        relationship.imports = true;
        if source.is_none() {
            source = Some(
                String::from_utf8_lossy(&provider.read_bytes(&files[file_index].abs_path)?)
                    .into_owned(),
            );
        }
        let source_text = source.as_deref().unwrap_or_default();
        if import_line_is_aliased(source_text, reference.line, reference.column, symbol) {
            relationship.aliased = true;
        }
    }
    Ok(relationship)
}

fn import_line_is_aliased(source: &str, line: usize, column: usize, symbol: &str) -> bool {
    let Some(text) = source.lines().nth(line.saturating_sub(1)) else {
        return false;
    };
    let start = column.saturating_sub(1);
    let Some(tail) = text.get(start + symbol.len()..) else {
        return false;
    };
    tail.trim_start().starts_with("as ")
}

pub(super) fn file_has_shadowing_signal<P: CatalogProvider + ?Sized>(
    provider: &P,
    file: &crate::repomap::IndexedFile,
    symbol: &str,
) -> Result<bool, NerveError> {
    if file.symbols.iter().any(|item| item.name == symbol) {
        return Ok(true);
    }
    let source = String::from_utf8_lossy(&provider.read_bytes(&file.abs_path)?).into_owned();
    Ok(source_has_shadowing_binding(
        &source,
        &file.language,
        symbol,
    ))
}

fn source_has_shadowing_binding(source: &str, language: &str, symbol: &str) -> bool {
    source.lines().any(|line| {
        let line = strip_shadow_comment(line, language).trim_start();
        if language == "python" && python_assignment_starts_with_symbol(line, symbol) {
            return true;
        }
        has_shadowing_prefix(line, language, symbol)
    })
}

fn strip_shadow_comment<'a>(line: &'a str, language: &str) -> &'a str {
    let marker = match language {
        "python" | "ruby" => "#",
        _ => "//",
    };
    line.split_once(marker).map_or(line, |(before, _)| before)
}

fn has_shadowing_prefix(line: &str, language: &str, symbol: &str) -> bool {
    let prefixes: &[&str] = match language {
        "rust" => &[
            "fn ", "let ", "let mut ", "const ", "static ", "struct ", "enum ", "trait ", "mod ",
            "type ",
        ],
        "python" => &["def ", "class "],
        "javascript" | "typescript" | "tsx" => &[
            "function ",
            "const ",
            "let ",
            "var ",
            "class ",
            "interface ",
            "type ",
        ],
        _ => &[],
    };
    prefixes
        .iter()
        .any(|prefix| starts_with_symbol_after_prefix(line, prefix, symbol))
}

fn python_assignment_starts_with_symbol(line: &str, symbol: &str) -> bool {
    if !line.starts_with(symbol) {
        return false;
    }
    let rest = &line[symbol.len()..];
    let rest = rest.trim_start();
    rest.starts_with('=') || rest.starts_with(':')
}

fn starts_with_symbol_after_prefix(line: &str, prefix: &str, symbol: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    if !rest.starts_with(symbol) {
        return false;
    }
    rest[symbol.len()..]
        .chars()
        .next()
        .is_none_or(|ch| !(ch == '_' || ch.is_ascii_alphanumeric()))
}
