use crate::{cancel::CancelToken, codemap::CodeReference, models::NerveError};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    analysis::IndexedFile,
    imports::resolve_import_reference,
    language::{
        is_high_document_frequency, is_reference_stopword, language_family, language_file_counts,
    },
};

const IMPORT_EDGE_WEIGHT: f64 = 8.0;

#[derive(Debug)]
pub(super) struct ReferenceGraph {
    pub(crate) edges: Vec<Vec<(usize, f64)>>,
    pub(crate) symbols_indexed: usize,
    pub(crate) edge_count: usize,
}

impl ReferenceGraph {
    pub(super) fn build(files: &[IndexedFile]) -> Self {
        Self::build_cancellable(files, &CancelToken::never()).expect("never-cancel token")
    }

    pub(super) fn build_cancellable(
        files: &[IndexedFile],
        cancel: &CancelToken,
    ) -> Result<Self, NerveError> {
        let language_file_counts = language_file_counts(files);
        let definitions = definition_index(files, &language_file_counts);
        let mut edge_maps = vec![BTreeMap::<usize, f64>::new(); files.len()];

        for (referencer_idx, file) in files.iter().enumerate() {
            cancel.check_cancelled()?;
            let mut references = file.references.clone();
            references
                .sort_by(|left, right| reference_sort_key(left).cmp(&reference_sort_key(right)));
            for reference in &references {
                let reference_language =
                    language_family(reference.effective_language(&file.language));
                if is_reference_stopword(&reference.name, reference_language) {
                    continue;
                }

                if reference.kind == "import"
                    && let Some(definer_idx) =
                        resolve_import_reference(files, referencer_idx, reference)
                    && definer_idx != referencer_idx
                {
                    *edge_maps[referencer_idx].entry(definer_idx).or_insert(0.0) +=
                        IMPORT_EDGE_WEIGHT;
                }

                let Some(definers) = definitions
                    .get(reference_language)
                    .and_then(|by_name| by_name.get(reference.name.as_str()))
                else {
                    continue;
                };
                for definer_idx in definers {
                    if *definer_idx == referencer_idx {
                        continue;
                    }
                    *edge_maps[referencer_idx].entry(*definer_idx).or_insert(0.0) += 1.0;
                }
            }
        }

        let edge_count = edge_maps.iter().map(BTreeMap::len).sum();
        let edges = edge_maps
            .into_iter()
            .map(|map| map.into_iter().collect())
            .collect();

        Ok(Self {
            edges,
            symbols_indexed: definitions
                .values()
                .flat_map(BTreeMap::values)
                .map(BTreeSet::len)
                .sum(),
            edge_count,
        })
    }
}

fn definition_index(
    files: &[IndexedFile],
    language_file_counts: &BTreeMap<String, usize>,
) -> BTreeMap<String, BTreeMap<String, BTreeSet<usize>>> {
    let mut definitions: BTreeMap<String, BTreeMap<String, BTreeSet<usize>>> = BTreeMap::new();
    for (idx, file) in files.iter().enumerate() {
        for symbol in &file.symbols {
            if !is_reference_stopword(&symbol.name, language_family(&file.language)) {
                definitions
                    .entry(language_family(&file.language).to_string())
                    .or_default()
                    .entry(symbol.name.clone())
                    .or_default()
                    .insert(idx);
            }
        }
    }

    for (language, by_name) in &mut definitions {
        let file_count = language_file_counts
            .get(language)
            .copied()
            .unwrap_or_default();
        by_name.retain(|_, definers| !is_high_document_frequency(definers.len(), file_count));
    }

    definitions
}

fn reference_sort_key(
    reference: &CodeReference,
) -> (&str, &str, usize, Option<&str>, Option<&str>) {
    (
        reference.kind.as_str(),
        reference.name.as_str(),
        reference.line,
        reference.import_path.as_deref(),
        reference.language.as_deref(),
    )
}
