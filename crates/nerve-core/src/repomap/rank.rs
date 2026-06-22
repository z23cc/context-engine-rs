use crate::{cancel::CancelToken, models::NerveError};
use std::{cmp::Ordering, collections::BTreeSet, path::Path};

use super::{analysis::IndexedFile, query::text_matches_terms};

pub(super) const DAMPING: f64 = 0.85;
pub(super) const ITERATIONS: usize = 30;

pub(super) fn page_rank_cancellable(
    edges: &[Vec<(usize, f64)>],
    personalization: &[f64],
    cancel: &CancelToken,
) -> Result<Vec<f64>, NerveError> {
    let n = edges.len();
    if n == 0 {
        return Ok(Vec::new());
    }

    let mut ranks = personalization.to_vec();
    let mut next = vec![0.0; n];

    for _ in 0..ITERATIONS {
        cancel.check_cancelled()?;
        next.fill(0.0);
        let mut dangling = 0.0;

        for (source_idx, outgoing) in edges.iter().enumerate() {
            let rank = ranks[source_idx];
            if outgoing.is_empty() {
                dangling += rank;
                continue;
            }

            let total_weight: f64 = outgoing.iter().map(|(_, weight)| *weight).sum();
            if total_weight == 0.0 {
                dangling += rank;
                continue;
            }

            for (target_idx, weight) in outgoing {
                next[*target_idx] += rank * (*weight / total_weight);
            }
        }

        for idx in 0..n {
            next[idx] = (1.0 - DAMPING) * personalization[idx]
                + DAMPING * (next[idx] + dangling * personalization[idx]);
        }

        std::mem::swap(&mut ranks, &mut next);
    }

    Ok(ranks)
}

pub(super) fn personalization(n: usize, seed_indices: &BTreeSet<usize>) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    if seed_indices.is_empty() {
        return vec![1.0 / n as f64; n];
    }

    let seed_weight = 1.0 / seed_indices.len() as f64;
    let mut vector = vec![0.0; n];
    for idx in seed_indices {
        vector[*idx] = seed_weight;
    }
    vector
}

pub(super) fn seed_indices(
    files: &[IndexedFile],
    seed_paths: &[String],
    query_terms: &BTreeSet<String>,
) -> BTreeSet<usize> {
    let mut seeds = BTreeSet::new();
    for (idx, file) in files.iter().enumerate() {
        if file.query_match
            || seed_paths.iter().any(|seed| path_matches_seed(file, seed))
            || (seed_paths.is_empty() && file_matches_query_terms(file, query_terms))
        {
            seeds.insert(idx);
        }
    }
    seeds
}

fn file_matches_query_terms(file: &IndexedFile, query_terms: &BTreeSet<String>) -> bool {
    text_matches_terms(&file.path, query_terms)
        || file
            .symbols
            .iter()
            .any(|symbol| text_matches_terms(&symbol.name, query_terms))
}

fn path_matches_seed(file: &IndexedFile, seed: &str) -> bool {
    if seed.is_empty() {
        return false;
    }
    file.path == seed
        || file.path.starts_with(&format!("{seed}/"))
        || file.abs_path == Path::new(seed)
        || file.abs_path.starts_with(Path::new(seed))
}

pub(super) fn score_cmp(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}
