use super::*;
use crate::codemap::CodeSymbol;
use std::{cmp::Ordering, collections::BTreeSet, path::PathBuf};

const NAME_WEIGHT: usize = 80;
const KIND_WEIGHT: usize = 24;
const SIGNATURE_WEIGHT: usize = 16;
const MEMBER_WEIGHT: usize = 10;
const MEMBER_SIGNATURE_WEIGHT: usize = 4;
const PATH_WEIGHT: usize = 4;

struct PendingSymbolMatch {
    item: SymbolSearchMatch,
    abs_path: PathBuf,
}

/// Fuzzy/partial symbol discovery across the catalog.
pub fn symbol_search<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &SymbolSearchRequest,
) -> Result<SymbolSearchResponse, NerveError> {
    symbol_search_cancellable(
        provider,
        &owned_arc(snapshot),
        request,
        &CancelToken::never(),
    )
}

/// Cancellable [`symbol_search`].
pub fn symbol_search_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &SymbolSearchRequest,
    cancel: &CancelToken,
) -> Result<SymbolSearchResponse, NerveError> {
    let terms = query_terms(&request.query);
    if terms.is_empty() {
        return Ok(empty_response(request));
    }

    let files = shared_indexed_files(provider, snapshot, cancel)?;
    let mut matches = Vec::new();
    for file in files.iter() {
        cancel.check_cancelled()?;
        if !language_matches(request.language.as_deref(), &file.language) {
            continue;
        }
        for symbol in &file.symbols {
            if !kind_matches(request.kind.as_deref(), &symbol.kind) {
                continue;
            }
            if let Some((score, matched_terms)) = score_symbol(&terms, file, symbol) {
                matches.push(PendingSymbolMatch {
                    item: SymbolSearchMatch {
                        name: symbol.name.clone(),
                        path: file.path.clone(),
                        display_path: file.display_path.clone(),
                        line: symbol.line,
                        kind: symbol.kind.clone(),
                        language: file.language.clone(),
                        score,
                        signature: symbol.signature.clone(),
                        text: None,
                        matched_terms,
                    },
                    abs_path: file.abs_path.clone(),
                });
            }
        }
    }

    matches.sort_by(|left, right| symbol_match_cmp(&left.item, &right.item));
    let total = matches.len();
    let truncated = total > request.max_results;
    matches.truncate(request.max_results);
    let mut sources = Sources::new(provider);
    for pending in &mut matches {
        pending.item.text = sources.line(&pending.item.path, &pending.abs_path, pending.item.line);
    }
    Ok(SymbolSearchResponse {
        query: request.query.clone(),
        matches: matches.into_iter().map(|pending| pending.item).collect(),
        total,
        truncated,
        note: NAV_NOTE.to_string(),
    })
}

fn empty_response(request: &SymbolSearchRequest) -> SymbolSearchResponse {
    SymbolSearchResponse {
        query: request.query.clone(),
        matches: Vec::new(),
        total: 0,
        truncated: false,
        note: NAV_NOTE.to_string(),
    }
}

fn kind_matches(filter: Option<&str>, kind: &str) -> bool {
    filter.is_none_or(|wanted| wanted.eq_ignore_ascii_case(kind))
}

fn score_symbol(
    terms: &[String],
    file: &IndexedFile,
    symbol: &CodeSymbol,
) -> Option<(usize, Vec<String>)> {
    let mut score = 0usize;
    let mut matched_terms = Vec::new();
    for term in terms {
        let term_score = score_term(term, file, symbol);
        if term_score > 0 {
            score += term_score;
            matched_terms.push(term.clone());
        }
    }
    (matched_terms.len() == terms.len()).then_some((score, matched_terms))
}

fn score_term(term: &str, file: &IndexedFile, symbol: &CodeSymbol) -> usize {
    let mut score = field_score(term, &symbol.name, NAME_WEIGHT)
        + field_score(term, &symbol.kind, KIND_WEIGHT)
        + field_score(term, &file.display_path, PATH_WEIGHT)
        + field_score(term, &file.path, PATH_WEIGHT);
    if let Some(signature) = &symbol.signature {
        score += field_score(term, signature, SIGNATURE_WEIGHT);
    }
    for member in &symbol.members {
        score += field_score(term, &member.name, MEMBER_WEIGHT);
        if let Some(signature) = &member.signature {
            score += field_score(term, signature, MEMBER_SIGNATURE_WEIGHT);
        }
    }
    score
}

fn field_score(term: &str, field: &str, weight: usize) -> usize {
    let lower = field.to_ascii_lowercase();
    if lower == term {
        return weight * 4;
    }
    let tokens = field_terms(field);
    if tokens.iter().any(|token| token == term) {
        return weight * 2;
    }
    if term.len() >= 2 && tokens.iter().any(|token| token.starts_with(term)) {
        return weight;
    }
    if term.len() >= 2 && lower.contains(term) {
        return weight / 2;
    }
    0
}

fn query_terms(query: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

fn field_terms(field: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut prev_lower_or_digit = false;
    let mut prev_upper = false;
    let chars: Vec<char> = field.chars().collect();
    for (index, ch) in chars.iter().copied().enumerate() {
        if !ch.is_ascii_alphanumeric() {
            push_term(&mut terms, &mut current);
            prev_lower_or_digit = false;
            prev_upper = false;
            continue;
        }
        let next_lower = chars
            .get(index + 1)
            .is_some_and(|next| next.is_ascii_lowercase());
        if ch.is_ascii_uppercase() && (prev_lower_or_digit || (prev_upper && next_lower)) {
            push_term(&mut terms, &mut current);
        }
        current.push(ch.to_ascii_lowercase());
        prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        prev_upper = ch.is_ascii_uppercase();
    }
    push_term(&mut terms, &mut current);
    terms
}

fn push_term(terms: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        terms.push(std::mem::take(current));
    }
}

fn symbol_match_cmp(left: &SymbolSearchMatch, right: &SymbolSearchMatch) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.display_path.cmp(&right.display_path))
        .then_with(|| left.line.cmp(&right.line))
        .then_with(|| left.kind.cmp(&right.kind))
        .then_with(|| left.name.cmp(&right.name))
}
