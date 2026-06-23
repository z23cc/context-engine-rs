use super::call_hierarchy::{SpanCache, enclosing_symbol};
use super::impact::target_path_matches;
use super::references::reference_confidence;
use super::*;
use crate::codemap::CodeSymbol;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReferencingKey {
    display_path: String,
    line: usize,
    reference_line: usize,
    reference_column: usize,
    symbol: String,
}

/// Find enclosing symbols that reference `symbol`.
pub fn find_referencing_symbols<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &FindReferencingSymbolsRequest,
) -> Result<FindReferencingSymbolsResponse, NerveError> {
    find_referencing_symbols_cancellable(
        provider,
        &owned_arc(snapshot),
        request,
        &CancelToken::never(),
    )
}

/// Cancellable [`find_referencing_symbols`].
pub fn find_referencing_symbols_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &FindReferencingSymbolsRequest,
    cancel: &CancelToken,
) -> Result<FindReferencingSymbolsResponse, NerveError> {
    let files = shared_indexed_files(provider, snapshot, cancel)?;
    let mut sources = Sources::new(provider);
    let mut spans = SpanCache::default();
    let definitions = target_definitions(&files, &mut sources, request);
    let definition_count = ambiguous_definition_count(&files, request);
    let context_lines = request.context_lines.min(5);
    let mut referencing = collect_referencing_symbols(
        &files,
        &mut sources,
        &mut spans,
        request,
        definition_count <= 1,
        context_lines,
    );
    referencing.sort_by(referencing_symbol_cmp);
    referencing.dedup_by_key(referencing_key);
    let max_results = request.max_results.max(1);
    let total = referencing.len();
    let truncated = total > max_results;
    referencing.truncate(max_results);
    Ok(FindReferencingSymbolsResponse {
        symbol: request.symbol.clone(),
        definitions,
        referencing_symbols: referencing,
        definition_count,
        total,
        truncated,
        context_lines,
        note: referencing_note(total),
    })
}

fn collect_referencing_symbols<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &FindReferencingSymbolsRequest,
    unambiguous: bool,
    context_lines: usize,
) -> Vec<ReferencingSymbol> {
    let def_files = target_definition_file_indexes(files, request);
    if def_files.is_empty() {
        return Vec::new();
    }
    let mut symbols = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let confidence = reference_confidence(files, idx, &def_files, unambiguous);
        collect_file_referencing_symbols(
            file,
            sources,
            spans,
            request,
            confidence,
            context_lines,
            &mut symbols,
        );
    }
    symbols
}

fn collect_file_referencing_symbols<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &FindReferencingSymbolsRequest,
    confidence: Confidence,
    context_lines: usize,
    out: &mut Vec<ReferencingSymbol>,
) {
    for reference in &file.references {
        if reference.has_embedded_language() || reference.name != request.symbol {
            continue;
        }
        let reference_language = reference.effective_language(&file.language);
        if !language_matches(request.language.as_deref(), reference_language) {
            continue;
        }
        if request.confident_only && confidence == Confidence::Low {
            continue;
        }
        let Some(enclosing) = enclosing_symbol(file, sources, spans, reference.line) else {
            continue;
        };
        out.push(referencing_symbol(
            file,
            sources,
            enclosing,
            reference,
            context_lines,
            confidence,
        ));
    }
}

fn referencing_symbol<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    symbol: &CodeSymbol,
    reference: &crate::codemap::CodeReference,
    context_lines: usize,
    confidence: Confidence,
) -> ReferencingSymbol {
    ReferencingSymbol {
        symbol: symbol.name.clone(),
        path: file.path.clone(),
        display_path: file.display_path.clone(),
        line: symbol.line,
        column: symbol.column,
        kind: symbol.kind.clone(),
        language: file.language.clone(),
        reference_line: reference.line,
        reference_column: reference.column,
        reference_kind: reference.kind.clone(),
        confidence,
        text: sources.line(&file.path, &file.abs_path, symbol.line),
        reference_text: sources.line(&file.path, &file.abs_path, reference.line),
        reference_context: reference_context(sources, file, reference.line, context_lines),
    }
}

fn reference_context<P: CatalogProvider + Sync>(
    sources: &mut Sources<P>,
    file: &IndexedFile,
    line: usize,
    context_lines: usize,
) -> Option<String> {
    if context_lines == 0 {
        return None;
    }
    let source = sources.source(&file.path, &file.abs_path)?;
    let total = source.lines().count();
    let start = line.saturating_sub(context_lines).max(1);
    let end = line.saturating_add(context_lines).min(total);
    if total == 0 || end < start {
        return None;
    }
    let mut out = String::new();
    for (idx, text) in source
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(end - start + 1)
    {
        out.push_str(&format!("{}: {}\n", idx + 1, text));
    }
    (!out.is_empty()).then_some(out)
}

fn target_definitions<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    request: &FindReferencingSymbolsRequest,
) -> Vec<SymbolLocation> {
    let mut definitions = Vec::new();
    for file in files {
        for symbol in &file.symbols {
            if target_matches_definition(request, file, symbol) {
                definitions.push(SymbolLocation {
                    path: file.path.clone(),
                    display_path: file.display_path.clone(),
                    line: symbol.line,
                    column: symbol.column,
                    kind: symbol.kind.clone(),
                    language: file.language.clone(),
                    signature: symbol.signature.clone(),
                    text: sources.line(&file.path, &file.abs_path, symbol.line),
                });
            }
        }
    }
    sort_locations(&mut definitions);
    definitions
}

fn target_definition_file_indexes(
    files: &[IndexedFile],
    request: &FindReferencingSymbolsRequest,
) -> HashSet<usize> {
    files
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            file.symbols
                .iter()
                .any(|symbol| target_matches_definition(request, file, symbol))
        })
        .map(|(idx, _)| idx)
        .collect()
}

fn ambiguous_definition_count(
    files: &[IndexedFile],
    request: &FindReferencingSymbolsRequest,
) -> usize {
    files
        .iter()
        .map(|file| {
            file.symbols
                .iter()
                .filter(|symbol| target_matches_ambiguity_scope(request, file, symbol))
                .count()
        })
        .sum()
}

fn target_matches_definition(
    request: &FindReferencingSymbolsRequest,
    file: &IndexedFile,
    symbol: &CodeSymbol,
) -> bool {
    target_matches_ambiguity_scope(request, file, symbol)
        && request
            .path
            .as_deref()
            .is_none_or(|path| target_path_matches(path, file, None))
}

fn target_matches_ambiguity_scope(
    request: &FindReferencingSymbolsRequest,
    file: &IndexedFile,
    symbol: &CodeSymbol,
) -> bool {
    symbol.name == request.symbol
        && language_matches(request.language.as_deref(), &file.language)
        && request
            .kind
            .as_deref()
            .is_none_or(|kind| kind.eq_ignore_ascii_case(&symbol.kind))
}

fn referencing_symbol_cmp(a: &ReferencingSymbol, b: &ReferencingSymbol) -> std::cmp::Ordering {
    a.display_path
        .cmp(&b.display_path)
        .then(a.line.cmp(&b.line))
        .then(a.symbol.cmp(&b.symbol))
        .then(a.reference_line.cmp(&b.reference_line))
        .then(a.reference_column.cmp(&b.reference_column))
}

fn referencing_key(item: &mut ReferencingSymbol) -> ReferencingKey {
    ReferencingKey {
        display_path: item.display_path.clone(),
        line: item.line,
        reference_line: item.reference_line,
        reference_column: item.reference_column,
        symbol: item.symbol.clone(),
    }
}

fn referencing_note(total: usize) -> String {
    if total == 0 {
        return format!("no enclosing symbols reference the target; {NAV_NOTE}");
    }
    format!("enclosing symbols that reference the target, with reference context; {NAV_NOTE}")
}
