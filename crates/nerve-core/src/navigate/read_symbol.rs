use super::*;
use crate::codemap::CodeSymbol;

#[derive(Debug, Clone)]
struct PendingSymbolRead {
    location: SymbolLocation,
    abs_path: std::path::PathBuf,
}

/// Read one symbol definition body/block from the catalog.
pub fn read_symbol<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ReadSymbolRequest,
) -> Result<ReadSymbolResponse, NerveError> {
    read_symbol_cancellable(
        provider,
        &owned_arc(snapshot),
        request,
        &CancelToken::never(),
    )
}

/// Cancellable [`read_symbol`].
pub fn read_symbol_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &ReadSymbolRequest,
    cancel: &CancelToken,
) -> Result<ReadSymbolResponse, NerveError> {
    let files = shared_indexed_files(provider, snapshot, cancel)?;
    let mut sources = Sources::new(provider);
    let mut matches = collect_matches(&files, &mut sources, request, cancel)?;
    matches.sort_by(|left, right| symbol_location_cmp(&left.location, &right.location));

    let total = matches.len();
    let max_matches = request.max_matches.max(1);
    let truncated = total > max_matches;
    matches.truncate(max_matches);
    let mut response = ReadSymbolResponse {
        symbol: request.symbol.clone(),
        body: None,
        matches: matches
            .iter()
            .map(|pending| pending.location.clone())
            .collect(),
        total,
        truncated,
        note: read_symbol_note(total, truncated, max_matches),
    };

    if request.include_body
        && total == 1
        && let Some(pending) = matches.first()
    {
        response.body = symbol_body(&mut sources, pending)?;
    }
    Ok(response)
}

fn collect_matches<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    request: &ReadSymbolRequest,
    cancel: &CancelToken,
) -> Result<Vec<PendingSymbolRead>, NerveError> {
    let mut matches = Vec::new();
    for file in files {
        cancel.check_cancelled()?;
        if !file_matches(request, file) {
            continue;
        }
        for symbol in &file.symbols {
            if !symbol_matches(request, symbol) {
                continue;
            }
            matches.push(PendingSymbolRead {
                location: symbol_location(file, sources, symbol),
                abs_path: file.abs_path.clone(),
            });
        }
    }
    Ok(matches)
}

fn file_matches(request: &ReadSymbolRequest, file: &IndexedFile) -> bool {
    language_matches(request.language.as_deref(), &file.language)
        && request
            .path
            .as_deref()
            .is_none_or(|path| path_matches(path, file))
}

fn path_matches(path: &str, file: &IndexedFile) -> bool {
    let wanted = path.trim().trim_matches('/');
    if wanted.is_empty() {
        return true;
    }
    file.path == wanted
        || file.display_path == wanted
        || file.path.starts_with(&format!("{wanted}/"))
        || file.display_path.starts_with(&format!("{wanted}/"))
}

fn symbol_matches(request: &ReadSymbolRequest, symbol: &CodeSymbol) -> bool {
    symbol.name == request.symbol
        && request
            .kind
            .as_deref()
            .is_none_or(|kind| kind.eq_ignore_ascii_case(&symbol.kind))
}

fn symbol_location<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    symbol: &CodeSymbol,
) -> SymbolLocation {
    SymbolLocation {
        path: file.path.clone(),
        display_path: file.display_path.clone(),
        line: symbol.line,
        column: symbol.column,
        kind: symbol.kind.clone(),
        language: file.language.clone(),
        signature: symbol.signature.clone(),
        text: sources.line(&file.path, &file.abs_path, symbol.line),
    }
}

fn symbol_body<P: CatalogProvider + Sync>(
    sources: &mut Sources<P>,
    pending: &PendingSymbolRead,
) -> Result<Option<ReadSymbolBody>, NerveError> {
    let Some(source) = sources.source(&pending.location.path, &pending.abs_path) else {
        return Ok(None);
    };
    let span = symbol_span(&pending.location.path, source, pending.location.line);
    let content = content_for_span(source, span);
    Ok(Some(ReadSymbolBody {
        path: pending.location.path.clone(),
        display_path: pending.location.display_path.clone(),
        start_line: span.0,
        end_line: span.1,
        kind: pending.location.kind.clone(),
        language: pending.location.language.clone(),
        signature: pending.location.signature.clone(),
        content,
    }))
}

fn symbol_span(path: &str, source: &str, line: usize) -> (usize, usize) {
    let total_lines = source.split_inclusive('\n').count().max(1);
    let span = block_span(path, source, line)
        .filter(|span| span.1 >= span.0)
        .or_else(|| {
            containing_block_span(path, source, line, line)
                .ok()
                .flatten()
        })
        .unwrap_or((line, line));
    clamp_span(span, total_lines)
}

fn clamp_span((first, last): (usize, usize), total_lines: usize) -> (usize, usize) {
    let first = first.max(1).min(total_lines.max(1));
    let last = last.max(first).min(total_lines.max(first));
    (first, last)
}

fn content_for_span(source: &str, (first, last): (usize, usize)) -> String {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    if lines.is_empty() {
        return String::new();
    }
    lines[first - 1..last].concat()
}

fn read_symbol_note(total: usize, truncated: bool, max_matches: usize) -> String {
    if total == 1 {
        return NAV_NOTE.to_string();
    }
    let mut note = if total == 0 {
        "no matching symbol definition found".to_string()
    } else {
        "ambiguous symbol definition; refine with path, language, or kind".to_string()
    };
    if truncated {
        note.push_str(&format!("; showing {max_matches} of {total} matches"));
    }
    note.push_str("; ");
    note.push_str(NAV_NOTE);
    note
}

fn symbol_location_cmp(left: &SymbolLocation, right: &SymbolLocation) -> std::cmp::Ordering {
    left.display_path
        .cmp(&right.display_path)
        .then(left.line.cmp(&right.line))
        .then(left.kind.cmp(&right.kind))
        .then(left.signature.cmp(&right.signature))
}
