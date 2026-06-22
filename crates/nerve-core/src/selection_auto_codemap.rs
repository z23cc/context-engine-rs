use crate::{
    codemap::{CodeReference, CodeSymbol, ParsedCodeFile},
    models::{CatalogEntry, NerveError},
    port::CatalogProvider,
    selection::{
        ManageSelectionMode, ManageSelectionOp, ManageSelectionRequest, Selection, SelectionKey,
        SelectionMode, has_selection_targets, selection_key, target_keys,
    },
    snapshot::CatalogSnapshot,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const AUTO_CODEMAP_LIMIT: usize = 8;

pub(crate) fn auto_expand_codemap<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &mut Selection,
    request: &ManageSelectionRequest,
) -> Result<usize, NerveError> {
    if !should_auto_expand(request) || selection.files.is_empty() {
        return Ok(0);
    }

    let seed_keys = auto_expand_seed_keys(snapshot, selection, request);
    if seed_keys.is_empty() {
        return Ok(0);
    }
    let seed_root_ids = seed_keys
        .iter()
        .map(|key| key.root_id.clone())
        .collect::<BTreeSet<_>>();
    let mut definitions = BTreeMap::<(String, String), BTreeSet<SelectionKey>>::new();
    let mut references = BTreeSet::<(String, String)>::new();

    for entry in &snapshot.entries {
        let key = selection_key(entry);
        let Some(parsed) = parsed_code_file(provider, entry)? else {
            continue;
        };
        if seed_keys.contains(&key) {
            collect_references(&mut references, &parsed.references, &parsed.language);
        } else if seed_root_ids.contains(&key.root_id) && !selection.files.contains_key(&key) {
            collect_definitions(&mut definitions, entry, &parsed.symbols, &parsed.language);
        }
    }

    let mut added = 0;
    for (language, name) in references {
        let Some(keys) = definitions.get(&(language, name.clone())) else {
            continue;
        };
        for key in keys {
            if selection.files.contains_key(key) {
                continue;
            }
            selection
                .files
                .insert(key.clone(), SelectionMode::CodemapOnly);
            added += 1;
            if added >= AUTO_CODEMAP_LIMIT {
                return Ok(added);
            }
        }
    }
    Ok(added)
}

fn should_auto_expand(request: &ManageSelectionRequest) -> bool {
    request.auto_codemap
        && matches!(
            request.op,
            ManageSelectionOp::Add | ManageSelectionOp::Set | ManageSelectionOp::Preview
        )
        && has_selection_targets(request)
        && request.mode != Some(ManageSelectionMode::CodemapOnly)
}

fn auto_expand_seed_keys(
    snapshot: &CatalogSnapshot,
    selection: &Selection,
    request: &ManageSelectionRequest,
) -> BTreeSet<SelectionKey> {
    target_keys(selection, snapshot, request)
        .into_iter()
        .filter(|key| {
            selection
                .files
                .get(key)
                .is_some_and(|mode| matches!(mode, SelectionMode::Full | SelectionMode::Slices(_)))
        })
        .collect()
}

fn parsed_code_file<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
) -> Result<Option<Arc<ParsedCodeFile>>, NerveError> {
    Ok(provider
        .code_symbols_for_path(&entry.abs_path, &entry.rel_path)?
        .ok()
        .flatten())
}

fn collect_definitions(
    definitions: &mut BTreeMap<(String, String), BTreeSet<SelectionKey>>,
    entry: &CatalogEntry,
    symbols: &[CodeSymbol],
    language: &str,
) {
    for symbol in symbols {
        if !is_type_like_symbol(&symbol.kind) {
            continue;
        }
        definitions
            .entry((language_family(language).to_string(), symbol.name.clone()))
            .or_default()
            .insert(selection_key(entry));
    }
}

fn collect_references(
    references: &mut BTreeSet<(String, String)>,
    file_references: &[CodeReference],
    fallback_language: &str,
) {
    for reference in file_references {
        references.insert((
            language_family(reference.effective_language(fallback_language)).to_string(),
            reference.name.clone(),
        ));
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
