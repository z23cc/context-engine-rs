use super::{BuildContextExcludedFile, format_score};
use crate::{
    CancelToken, CatalogEntry, CatalogProvider, CatalogSnapshot, CtxError, Selection,
    SelectionMode, WorkspaceContextInclude, WorkspaceContextRequest, repomap::IndexedFile,
    repomap::indexed_files_cancellable, selection::SelectionKey, workspace_context_for_selection,
};
use std::collections::{BTreeMap, BTreeSet};

const REFERENCE_EXPANSION_LIMIT: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ExpansionCandidate {
    name: String,
    path: String,
}

pub(super) fn expand_reference_codemap_selection<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &mut Selection,
    token_budget: usize,
    cancel: &CancelToken,
) -> Result<Vec<BuildContextExcludedFile>, CtxError> {
    if selection.files.is_empty() || token_budget == 0 {
        return Ok(Vec::new());
    }

    let indexed_files = indexed_files_cancellable(provider, snapshot, cancel)?;
    let selected_paths = selected_paths(selection);
    let candidate_paths = candidate_paths(expansion_candidates(&indexed_files, &selected_paths));
    let entries_by_path = entries_by_path(snapshot);
    let mut excluded = Vec::new();

    for path in candidate_paths.into_iter().take(REFERENCE_EXPANSION_LIMIT) {
        cancel.check_cancelled()?;
        let Some(entry) = entries_by_path.get(path.as_str()) else {
            continue;
        };
        if !try_add_codemap(provider, snapshot, selection, entry, token_budget)? {
            excluded.push(expansion_excluded_file(
                provider,
                entry,
                "reference_expansion_over_budget",
            ));
        }
    }

    Ok(excluded)
}

fn selected_paths(selection: &Selection) -> BTreeSet<String> {
    selection.files.keys().map(|key| key.path.clone()).collect()
}

fn entries_by_path(snapshot: &CatalogSnapshot) -> BTreeMap<&str, &CatalogEntry> {
    snapshot
        .entries
        .iter()
        .map(|entry| (entry.rel_path.as_str(), entry))
        .collect()
}

fn expansion_candidates(
    indexed_files: &[IndexedFile],
    selected_paths: &BTreeSet<String>,
) -> BTreeSet<ExpansionCandidate> {
    let definitions = definitions_by_name(indexed_files, selected_paths);
    let references = referenced_names(indexed_files, selected_paths);
    let mut candidates = BTreeSet::new();

    for (language, name) in references {
        if let Some(paths) = definitions.get(&(language, name.clone())) {
            for path in paths {
                candidates.insert(ExpansionCandidate {
                    name: name.clone(),
                    path: path.clone(),
                });
            }
        }
    }

    candidates
}

fn candidate_paths(candidates: BTreeSet<ExpansionCandidate>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut paths = Vec::new();
    for candidate in candidates {
        if seen.insert(candidate.path.clone()) {
            paths.push(candidate.path);
        }
    }
    paths
}

fn definitions_by_name(
    indexed_files: &[IndexedFile],
    selected_paths: &BTreeSet<String>,
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut definitions = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for file in indexed_files {
        if selected_paths.contains(&file.path) {
            continue;
        }
        for symbol in &file.symbols {
            if !is_type_like_symbol(&symbol.kind) {
                continue;
            }
            definitions
                .entry((
                    language_family(&file.language).to_string(),
                    symbol.name.clone(),
                ))
                .or_default()
                .insert(file.path.clone());
        }
    }
    definitions
}

fn referenced_names(
    indexed_files: &[IndexedFile],
    selected_paths: &BTreeSet<String>,
) -> BTreeSet<(String, String)> {
    let mut references = BTreeSet::new();
    for file in indexed_files {
        if !selected_paths.contains(&file.path) {
            continue;
        }
        for reference in &file.references {
            references.insert((
                language_family(&file.language).to_string(),
                reference.name.clone(),
            ));
        }
    }
    references
}

fn try_add_codemap<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &mut Selection,
    entry: &CatalogEntry,
    token_budget: usize,
) -> Result<bool, CtxError> {
    let key = selection_key(entry);
    if selection.files.contains_key(&key) {
        return Ok(true);
    }

    let mut next_selection = selection.clone();
    next_selection.files.insert(key, SelectionMode::CodemapOnly);
    let workspace = workspace_context_for_selection(
        provider,
        snapshot,
        &next_selection,
        &WorkspaceContextRequest {
            include: vec![
                WorkspaceContextInclude::FileMap,
                WorkspaceContextInclude::Contents,
            ],
            instructions: None,
        },
    )?;

    if workspace.tokens.total_tokens <= token_budget {
        *selection = next_selection;
        return Ok(true);
    }
    Ok(false)
}

fn selection_key(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

fn expansion_excluded_file<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    reason: &str,
) -> BuildContextExcludedFile {
    BuildContextExcludedFile {
        path: entry.rel_path.clone(),
        display_path: provider.display_path(&entry.abs_path),
        score: format_score(0.0),
        reason: reason.to_string(),
    }
}

fn is_type_like_symbol(kind: &str) -> bool {
    matches!(
        kind,
        "class" | "struct" | "enum" | "interface" | "trait" | "type" | "typedef" | "record"
    )
}

fn language_family(language: &str) -> &str {
    match language {
        "typescript" | "tsx" => "javascript",
        other => other,
    }
}
