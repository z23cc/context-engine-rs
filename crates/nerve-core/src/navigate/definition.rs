use super::*;

/// Find all definitions of `symbol` across the catalog.
pub fn goto_definition<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
) -> Result<DefinitionResponse, NerveError> {
    goto_definition_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Cancellable [`goto_definition`].
pub fn goto_definition_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
    cancel: &CancelToken,
) -> Result<DefinitionResponse, NerveError> {
    let files = indexed_files_cancellable(provider, snapshot, cancel)?;
    let mut sources = Sources::new(provider);
    let mut definitions = Vec::new();
    for file in &files {
        if !language_matches(request.language.as_deref(), &file.language) {
            continue;
        }
        for symbol in &file.symbols {
            if symbol.name == request.symbol {
                let text = sources.line(&file.path, &file.abs_path, symbol.line);
                definitions.push(SymbolLocation {
                    path: file.path.clone(),
                    display_path: file.display_path.clone(),
                    line: symbol.line,
                    column: symbol.column,
                    kind: symbol.kind.clone(),
                    language: file.language.clone(),
                    signature: symbol.signature.clone(),
                    text,
                });
            }
        }
    }
    sort_locations(&mut definitions);
    let total = definitions.len();
    let truncated = total > request.max_results;
    definitions.truncate(request.max_results);
    Ok(DefinitionResponse {
        symbol: request.symbol.clone(),
        definitions,
        total,
        truncated,
        note: NAV_NOTE.to_string(),
    })
}
