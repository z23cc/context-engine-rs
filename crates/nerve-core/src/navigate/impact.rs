use super::call_hierarchy::{SpanCache, enclosing_symbol};
use super::references::reference_confidence;
use super::*;
use crate::codemap::CodeSymbol;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ImpactKey {
    symbol: String,
    path: String,
    line: usize,
}

#[derive(Debug, Clone)]
struct ImpactTarget {
    name: String,
    path: Option<String>,
    line: Option<usize>,
    language: Option<String>,
    kind: Option<String>,
}

/// Analyze a bounded name-based reverse dependency graph for `symbol`.
pub fn analyze_impact<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ImpactAnalysisRequest,
) -> Result<ImpactAnalysisResponse, NerveError> {
    analyze_impact_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Cancellable [`analyze_impact`].
pub fn analyze_impact_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &ImpactAnalysisRequest,
    cancel: &CancelToken,
) -> Result<ImpactAnalysisResponse, NerveError> {
    let files = indexed_files_cancellable(provider, snapshot, cancel)?;
    let max_depth = request.max_depth.max(1);
    let max_results = request.max_results.max(1);
    let mut sources = Sources::new(provider);
    let mut spans = SpanCache::default();
    let definitions = seed_definitions(&files, &mut sources, request);
    let seed_keys = impact_keys_from_definitions(&definitions, &request.symbol);
    let impacted = impact_bfs(
        &files,
        &mut sources,
        &mut spans,
        request,
        max_depth,
        &seed_keys,
        cancel,
    )?;
    let total = impacted.len();
    let truncated = total > max_results;
    let impacted = impacted.into_iter().take(max_results).collect();

    Ok(ImpactAnalysisResponse {
        symbol: request.symbol.clone(),
        definitions,
        impacted,
        total,
        truncated,
        max_depth,
        note: impact_note(total),
    })
}

fn impact_bfs<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &ImpactAnalysisRequest,
    max_depth: usize,
    seed_keys: &HashSet<ImpactKey>,
    cancel: &CancelToken,
) -> Result<Vec<ImpactSymbol>, NerveError> {
    let mut queue = std::collections::VecDeque::from([seed_target(request)]);
    let mut visited_targets = HashSet::new();
    let mut seen_impacts = seed_keys.clone();
    let mut impacted = Vec::new();
    while let Some((target, depth)) = queue.pop_front() {
        cancel.check_cancelled()?;
        if !visited_targets.insert(target_key(&target)) || depth >= max_depth {
            continue;
        }
        let mut next = collect_dependents(
            files,
            sources,
            spans,
            request,
            &target,
            depth + 1,
            seed_keys,
        );
        next.sort_by(impact_symbol_cmp);
        for node in next {
            let key = ImpactKey::from(&node);
            if !seen_impacts.insert(key) {
                continue;
            }
            if node.depth < max_depth {
                queue.push_back((target_from_node(&node), node.depth));
            }
            impacted.push(node);
        }
    }
    impacted.sort_by(impact_symbol_cmp);
    Ok(impacted)
}

fn seed_target(request: &ImpactAnalysisRequest) -> (ImpactTarget, usize) {
    (
        ImpactTarget {
            name: request.symbol.clone(),
            path: request.path.clone(),
            line: None,
            language: request.language.clone(),
            kind: request.kind.clone(),
        },
        0,
    )
}

fn target_from_node(node: &ImpactSymbol) -> ImpactTarget {
    ImpactTarget {
        name: node.symbol.clone(),
        path: Some(node.path.clone()),
        line: Some(node.line),
        language: Some(node.language.clone()),
        kind: Some(node.kind.clone()),
    }
}

fn collect_dependents<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &ImpactAnalysisRequest,
    target: &ImpactTarget,
    depth: usize,
    seed_keys: &HashSet<ImpactKey>,
) -> Vec<ImpactSymbol> {
    let def_files = target_definition_file_indexes(files, target);
    if def_files.is_empty() {
        return Vec::new();
    }
    let unambiguous = count_ambiguous_definitions(files, target) <= 1;
    let mut nodes = Vec::new();
    for (idx, file) in files.iter().enumerate() {
        let confidence = reference_confidence(files, idx, &def_files, unambiguous);
        let mut file_nodes =
            collect_file_dependents(file, sources, spans, request, target, confidence, depth);
        file_nodes.retain(|node| !seed_keys.contains(&ImpactKey::from(node)));
        nodes.extend(file_nodes);
    }
    nodes
}

fn collect_file_dependents<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    spans: &mut SpanCache,
    request: &ImpactAnalysisRequest,
    target: &ImpactTarget,
    confidence: Confidence,
    depth: usize,
) -> Vec<ImpactSymbol> {
    let mut nodes = Vec::new();
    for reference in &file.references {
        if reference.has_embedded_language() || reference.name != target.name {
            continue;
        }
        let reference_language = reference.effective_language(&file.language);
        if !language_matches(request.language.as_deref(), reference_language) {
            continue;
        }
        if request.confident_only && confidence == Confidence::Low {
            continue;
        }
        let Some(caller) = enclosing_symbol(file, sources, spans, reference.line) else {
            continue;
        };
        if is_self_target(file, caller, target) {
            continue;
        }
        nodes.push(impact_symbol(
            file,
            sources,
            caller,
            reference,
            depth,
            &target.name,
            confidence,
        ));
    }
    nodes
}

fn impact_symbol<P: CatalogProvider + Sync>(
    file: &IndexedFile,
    sources: &mut Sources<P>,
    caller: &CodeSymbol,
    reference: &crate::codemap::CodeReference,
    depth: usize,
    via_symbol: &str,
    confidence: Confidence,
) -> ImpactSymbol {
    ImpactSymbol {
        symbol: caller.name.clone(),
        path: file.path.clone(),
        display_path: file.display_path.clone(),
        line: caller.line,
        column: caller.column,
        kind: caller.kind.clone(),
        language: file.language.clone(),
        depth,
        via_symbol: via_symbol.to_string(),
        reference_line: reference.line,
        reference_column: reference.column,
        reference_kind: reference.kind.clone(),
        confidence,
        text: sources.line(&file.path, &file.abs_path, caller.line),
    }
}

fn seed_definitions<P: CatalogProvider + Sync>(
    files: &[IndexedFile],
    sources: &mut Sources<P>,
    request: &ImpactAnalysisRequest,
) -> Vec<SymbolLocation> {
    let mut definitions = Vec::new();
    let target = seed_target(request).0;
    for file in files {
        for symbol in &file.symbols {
            if target_matches_definition(&target, file, symbol) {
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

fn target_definition_file_indexes(files: &[IndexedFile], target: &ImpactTarget) -> HashSet<usize> {
    files
        .iter()
        .enumerate()
        .filter(|(_, file)| {
            file.symbols
                .iter()
                .any(|symbol| target_matches_definition(target, file, symbol))
        })
        .map(|(idx, _)| idx)
        .collect()
}

fn count_ambiguous_definitions(files: &[IndexedFile], target: &ImpactTarget) -> usize {
    files
        .iter()
        .map(|file| {
            file.symbols
                .iter()
                .filter(|symbol| target_matches_ambiguity_scope(target, file, symbol))
                .count()
        })
        .sum()
}

fn target_matches_definition(
    target: &ImpactTarget,
    file: &IndexedFile,
    symbol: &CodeSymbol,
) -> bool {
    symbol.name == target.name
        && language_matches(target.language.as_deref(), &file.language)
        && target
            .kind
            .as_deref()
            .is_none_or(|kind| kind.eq_ignore_ascii_case(&symbol.kind))
        && target.line.is_none_or(|line| symbol.line == line)
        && target
            .path
            .as_deref()
            .is_none_or(|path| target_path_matches(path, file, target.line))
}

fn target_matches_ambiguity_scope(
    target: &ImpactTarget,
    file: &IndexedFile,
    symbol: &CodeSymbol,
) -> bool {
    symbol.name == target.name
        && language_matches(target.language.as_deref(), &file.language)
        && target
            .kind
            .as_deref()
            .is_none_or(|kind| kind.eq_ignore_ascii_case(&symbol.kind))
}

pub(super) fn target_path_matches(
    path: &str,
    file: &IndexedFile,
    exact_line: Option<usize>,
) -> bool {
    let wanted = path.trim().trim_matches('/');
    if wanted.is_empty() {
        return true;
    }
    file.path == wanted
        || file.display_path == wanted
        || (exact_line.is_none()
            && (file.path.starts_with(&format!("{wanted}/"))
                || file.display_path.starts_with(&format!("{wanted}/"))))
}

fn is_self_target(file: &IndexedFile, symbol: &CodeSymbol, target: &ImpactTarget) -> bool {
    target.path.as_deref().is_some_and(|path| file.path == path)
        && target.line.is_some_and(|line| symbol.line == line)
}

fn target_key(target: &ImpactTarget) -> (String, Option<String>, Option<usize>) {
    (target.name.clone(), target.path.clone(), target.line)
}

fn impact_keys_from_definitions(
    definitions: &[SymbolLocation],
    symbol: &str,
) -> HashSet<ImpactKey> {
    definitions
        .iter()
        .map(|definition| ImpactKey {
            symbol: symbol.to_string(),
            path: definition.path.clone(),
            line: definition.line,
        })
        .collect()
}

fn impact_symbol_cmp(a: &ImpactSymbol, b: &ImpactSymbol) -> std::cmp::Ordering {
    a.depth
        .cmp(&b.depth)
        .then(a.display_path.cmp(&b.display_path))
        .then(a.line.cmp(&b.line))
        .then(a.symbol.cmp(&b.symbol))
        .then(a.reference_line.cmp(&b.reference_line))
}

fn impact_note(total: usize) -> String {
    if total == 0 {
        return format!("no impacted enclosing symbols found; {NAV_NOTE}");
    }
    format!("bounded reverse dependency graph over enclosing symbols; {NAV_NOTE}")
}

impl From<&ImpactSymbol> for ImpactKey {
    fn from(node: &ImpactSymbol) -> Self {
        Self {
            symbol: node.symbol.clone(),
            path: node.path.clone(),
            line: node.line,
        }
    }
}
