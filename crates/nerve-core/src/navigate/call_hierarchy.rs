use super::*;

/// Build a name-based call hierarchy for `symbol`.
pub fn call_hierarchy<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &CallHierarchyRequest,
) -> Result<CallHierarchyResponse, NerveError> {
    call_hierarchy_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Cancellable [`call_hierarchy`].
///
/// Incoming: every reference to `symbol`, mapped to its enclosing definition
/// (the innermost symbol whose tree-sitter block span contains the reference
/// line) — the caller. Outgoing: the references inside `symbol`'s own block,
/// resolved by name to definitions — the callees. Both are name-based and
/// best-effort (see the response note).
pub fn call_hierarchy_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &CallHierarchyRequest,
    cancel: &CancelToken,
) -> Result<CallHierarchyResponse, NerveError> {
    let files = indexed_files_cancellable(provider, snapshot, cancel)?;
    let mut sources = Sources::new(provider);
    let mut span_cache = SpanCache::default();
    let want_incoming = matches!(
        request.direction,
        CallDirection::Incoming | CallDirection::Both
    );
    let want_outgoing = matches!(
        request.direction,
        CallDirection::Outgoing | CallDirection::Both
    );

    let incoming = if want_incoming {
        cancel.check_cancelled()?;
        incoming_call_edges(&files, &mut sources, &mut span_cache, request)
    } else {
        Vec::new()
    };
    let outgoing = if want_outgoing {
        cancel.check_cancelled()?;
        outgoing_call_edges(&files, &mut sources, &mut span_cache, request)
    } else {
        Vec::new()
    };

    Ok(CallHierarchyResponse {
        symbol: request.symbol.clone(),
        incoming,
        outgoing,
        note: NAV_NOTE.to_string(),
    })
}

#[derive(Default)]
pub(super) struct SpanCache(HashMap<(String, usize), Option<(usize, usize)>>);

impl SpanCache {
    fn span_of<P: CatalogProvider + Sync>(
        &mut self,
        sources: &mut Sources<P>,
        rel: &str,
        abs: &Path,
        line: usize,
    ) -> Option<(usize, usize)> {
        *self.0.entry((rel.to_string(), line)).or_insert_with(|| {
            sources
                .source(rel, abs)
                .and_then(|src| block_span(rel, src, line))
        })
    }
}

pub(super) fn incoming_call_edges<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &CallHierarchyRequest,
) -> Vec<CallEdge> {
    let mut incoming = Vec::new();
    for file in files {
        for reference in &file.references {
            if reference.has_embedded_language() {
                continue;
            }
            if !language_matches(
                request.language.as_deref(),
                reference.effective_language(&file.language),
            ) {
                continue;
            }
            if reference.name != request.symbol {
                continue;
            }
            if let Some(caller) = enclosing_symbol(file, sources, spans, reference.line) {
                incoming.push(call_edge_for_symbol(file, sources, caller));
            }
        }
    }
    finalize_call_edges(incoming, request.max_results)
}

pub(super) fn enclosing_symbol<'a, P: CatalogProvider + Sync>(
    file: &'a IndexedFile,
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    reference_line: usize,
) -> Option<&'a crate::codemap::CodeSymbol> {
    let mut best = None;
    let mut best_size = usize::MAX;
    for symbol in &file.symbols {
        if symbol.line > reference_line {
            continue;
        }
        let Some((start, end)) = spans.span_of(sources, &file.path, &file.abs_path, symbol.line)
        else {
            continue;
        };
        if start <= reference_line && reference_line <= end && (end - start) < best_size {
            best_size = end - start;
            best = Some(symbol);
        }
    }
    best
}

pub(super) fn call_edge_for_symbol<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    symbol: &crate::codemap::CodeSymbol,
) -> CallEdge {
    CallEdge {
        symbol: symbol.name.clone(),
        path: file.path.clone(),
        display_path: file.display_path.clone(),
        line: symbol.line,
        column: symbol.column,
        kind: symbol.kind.clone(),
        language: file.language.clone(),
        text: sources.line(&file.path, &file.abs_path, symbol.line),
    }
}

pub(super) fn outgoing_call_edges<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &CallHierarchyRequest,
) -> Vec<CallEdge> {
    let defs_by_name = definitions_by_name(files);
    let mut outgoing = Vec::new();
    for file in files {
        collect_outgoing_for_file(file, sources, spans, request, &defs_by_name, &mut outgoing);
    }
    let mut outgoing = finalize_call_edges(outgoing, request.max_results);
    fill_call_edge_snippets(&mut outgoing, files, sources);
    outgoing
}

pub(super) fn definitions_by_name(files: &[IndexedFile]) -> HashMap<&str, Vec<DefRef<'_>>> {
    let mut defs_by_name: HashMap<&str, Vec<DefRef>> = HashMap::new();
    for file in files {
        for symbol in &file.symbols {
            defs_by_name
                .entry(symbol.name.as_str())
                .or_default()
                .push(DefRef {
                    path: &file.path,
                    display_path: &file.display_path,
                    language: &file.language,
                    line: symbol.line,
                    column: symbol.column,
                    kind: &symbol.kind,
                });
        }
    }
    defs_by_name
}

pub(super) fn collect_outgoing_for_file<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &CallHierarchyRequest,
    defs_by_name: &HashMap<&str, Vec<DefRef<'_>>>,
    outgoing: &mut Vec<CallEdge>,
) {
    if !language_matches(request.language.as_deref(), &file.language) {
        return;
    }
    for symbol in &file.symbols {
        if symbol.name != request.symbol {
            continue;
        }
        let Some(span) = spans.span_of(sources, &file.path, &file.abs_path, symbol.line) else {
            continue;
        };
        collect_outgoing_in_span(file, span, request, defs_by_name, outgoing);
    }
}

pub(super) fn collect_outgoing_in_span(
    file: &IndexedFile,
    span: (usize, usize),
    request: &CallHierarchyRequest,
    defs_by_name: &HashMap<&str, Vec<DefRef<'_>>>,
    outgoing: &mut Vec<CallEdge>,
) {
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for reference in &file.references {
        if reference.line < span.0
            || reference.line > span.1
            || reference.name == request.symbol
            || !seen.insert(reference.name.as_str())
        {
            continue;
        }
        add_outgoing_targets(reference.name.as_str(), defs_by_name, outgoing);
    }
}

pub(super) fn add_outgoing_targets(
    symbol: &str,
    defs_by_name: &HashMap<&str, Vec<DefRef<'_>>>,
    outgoing: &mut Vec<CallEdge>,
) {
    let Some(targets) = defs_by_name.get(symbol) else {
        return;
    };
    for target in targets {
        outgoing.push(CallEdge {
            symbol: symbol.to_string(),
            path: target.path.to_string(),
            display_path: target.display_path.to_string(),
            line: target.line,
            column: target.column,
            kind: target.kind.to_string(),
            language: target.language.to_string(),
            text: None,
        });
    }
}

pub(super) fn finalize_call_edges(mut edges: Vec<CallEdge>, max_results: usize) -> Vec<CallEdge> {
    edges.sort_by(call_edge_cmp);
    edges.dedup();
    edges.truncate(max_results);
    edges
}

pub(super) fn fill_call_edge_snippets<P: CatalogProvider + Sync>(
    edges: &mut [CallEdge],
    files: &[IndexedFile],
    sources: &mut Sources<P>,
) {
    for edge in edges {
        if let Some(abs) = files
            .iter()
            .find(|file| file.path == edge.path)
            .map(|file| file.abs_path.clone())
        {
            edge.text = sources.line(&edge.path, &abs, edge.line);
        }
    }
}

/// A definition reference into the indexed file set, for callee resolution.
pub(super) struct DefRef<'a> {
    path: &'a str,
    display_path: &'a str,
    language: &'a str,
    line: usize,
    column: usize,
    kind: &'a str,
}

pub(super) fn call_edge_cmp(a: &CallEdge, b: &CallEdge) -> std::cmp::Ordering {
    a.display_path
        .cmp(&b.display_path)
        .then(a.line.cmp(&b.line))
        .then(a.column.cmp(&b.column))
        .then(a.symbol.cmp(&b.symbol))
}
