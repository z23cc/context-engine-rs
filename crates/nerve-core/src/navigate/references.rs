use super::*;

/// Find all references to `symbol` across the catalog.
pub fn find_references<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &NavigateRequest,
) -> Result<ReferencesResponse, NerveError> {
    find_references_cancellable(
        provider,
        &owned_arc(snapshot),
        request,
        &CancelToken::never(),
    )
}

/// Cancellable [`find_references`].
pub fn find_references_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &NavigateRequest,
    cancel: &CancelToken,
) -> Result<ReferencesResponse, NerveError> {
    let files = shared_indexed_files(provider, snapshot, cancel)?;
    let mut sources = Sources::new(provider);
    let definition_count = count_definitions(&files, request);
    let mut references = collect_references(&files, &mut sources, request);
    references.sort_by(reference_cmp);

    let total = references.len();
    let truncated = total > request.max_results;
    references.truncate(request.max_results);
    let definitions = collect_optional_definitions(&files, &mut sources, request);

    Ok(ReferencesResponse {
        symbol: request.symbol.clone(),
        references,
        definitions,
        definition_count,
        total,
        truncated,
        note: NAV_NOTE.to_string(),
    })
}

pub(super) fn count_definitions(files: &[IndexedFile], request: &NavigateRequest) -> usize {
    files
        .iter()
        .filter(|file| language_matches(request.language.as_deref(), &file.language))
        .map(|file| {
            file.symbols
                .iter()
                .filter(|symbol| symbol.name == request.symbol)
                .count()
        })
        .sum()
}

pub(super) fn collect_references<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    request: &NavigateRequest,
) -> Vec<ReferenceLocation> {
    let def_files = definition_file_indexes(files, request);
    let unambiguous = count_definitions(files, request) <= 1;
    let mut references = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let confidence = reference_confidence(files, idx, &def_files, unambiguous);
        collect_file_references(file, sources, request, confidence, &mut references);
    }
    references
}

pub(super) fn definition_file_indexes(
    files: &[IndexedFile],
    request: &NavigateRequest,
) -> HashSet<usize> {
    files
        .iter()
        .enumerate()
        .filter(|(_, file)| language_matches(request.language.as_deref(), &file.language))
        .filter(|(_, file)| {
            file.symbols
                .iter()
                .any(|symbol| symbol.name == request.symbol)
        })
        .map(|(idx, _)| idx)
        .collect()
}

pub(super) fn reference_confidence(
    files: &[IndexedFile],
    idx: usize,
    def_files: &HashSet<usize>,
    unambiguous: bool,
) -> Confidence {
    let imports_a_definer = files[idx].references.iter().any(|reference| {
        reference.kind == "import"
            && resolve_import_reference(files, idx, reference)
                .is_some_and(|target| def_files.contains(&target))
    });
    if unambiguous || def_files.contains(&idx) || imports_a_definer {
        Confidence::High
    } else {
        Confidence::Low
    }
}

pub(super) fn collect_file_references<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    request: &NavigateRequest,
    confidence: Confidence,
    references: &mut Vec<ReferenceLocation>,
) {
    for reference in &file.references {
        let reference_language = reference.effective_language(&file.language);
        if reference.name != request.symbol {
            continue;
        }
        if !language_matches(request.language.as_deref(), reference_language) {
            continue;
        }
        if request.confident_only && confidence == Confidence::Low {
            continue;
        }
        references.push(ReferenceLocation {
            path: file.path.clone(),
            display_path: file.display_path.clone(),
            line: reference.line,
            column: reference.column,
            kind: reference.kind.clone(),
            language: reference_language.to_string(),
            text: sources.line(&file.path, &file.abs_path, reference.line),
            confidence,
        });
    }
}

pub(super) fn reference_cmp(a: &ReferenceLocation, b: &ReferenceLocation) -> std::cmp::Ordering {
    a.display_path
        .cmp(&b.display_path)
        .then(a.line.cmp(&b.line))
        .then(a.kind.cmp(&b.kind))
}

pub(super) fn collect_optional_definitions<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    request: &NavigateRequest,
) -> Vec<SymbolLocation> {
    if !request.include_definitions {
        return Vec::new();
    }
    let mut defs = Vec::new();
    collect_definitions(files, sources, request, &mut defs);
    sort_locations(&mut defs);
    defs.truncate(request.max_results);
    defs
}

pub(super) fn collect_definitions<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    request: &NavigateRequest,
    defs: &mut Vec<SymbolLocation>,
) {
    for file in files {
        if !language_matches(request.language.as_deref(), &file.language) {
            continue;
        }
        for symbol in &file.symbols {
            if symbol.name == request.symbol {
                defs.push(SymbolLocation {
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
}
