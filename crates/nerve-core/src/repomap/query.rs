use std::{collections::BTreeSet, path::PathBuf};

const MIN_QUERY_TERM_LEN: usize = 3;
const QUERY_STOPWORDS: &[&str] = &[
    "api",
    "code",
    "common",
    "config",
    "data",
    "error",
    "file",
    "files",
    "find",
    "get",
    "handling",
    "module",
    "modules",
    "new",
    "old",
    "project",
    "repo",
    "repository",
    "set",
    "test",
    "tests",
    "util",
    "utils",
];

pub(super) fn normalized_query(query: Option<&str>) -> Option<String> {
    query
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(ToString::to_string)
}

pub(super) fn query_terms(query: Option<&str>) -> BTreeSet<String> {
    query.map(identifier_terms).unwrap_or_default()
}

pub(super) fn text_matches_terms(text: &str, terms: &BTreeSet<String>) -> bool {
    if terms.is_empty() {
        return false;
    }
    identifier_terms(text)
        .iter()
        .any(|term| terms.contains(term))
}

pub(super) fn normalize_seed_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .map(|path| {
            path.trim_start_matches("./")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|path| !path.is_empty())
        .collect()
}

pub(super) fn query_matches(path: &str, source: &str, query: &str) -> bool {
    let case_sensitive = query.chars().any(char::is_uppercase);
    if case_sensitive {
        return path.contains(query) || source.contains(query);
    }
    let query = query.to_ascii_lowercase();
    path.to_ascii_lowercase().contains(&query) || source.to_ascii_lowercase().contains(&query)
}

fn identifier_terms(text: &str) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for raw in text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
        insert_term(raw, &mut terms);
        for part in raw.split('_') {
            insert_term(part, &mut terms);
            insert_camel_parts(part, &mut terms);
        }
    }
    terms
}

fn insert_camel_parts(text: &str, terms: &mut BTreeSet<String>) {
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0usize;
    for idx in 1..chars.len() {
        let prev = chars[idx - 1];
        let current = chars[idx];
        let next = chars.get(idx + 1).copied();
        if camel_boundary(prev, current, next) {
            insert_term(&chars[start..idx].iter().collect::<String>(), terms);
            start = idx;
        }
    }
    insert_term(&chars[start..].iter().collect::<String>(), terms);
}

fn camel_boundary(prev: char, current: char, next: Option<char>) -> bool {
    if !current.is_ascii_uppercase() {
        return false;
    }
    prev.is_ascii_lowercase()
        || prev.is_ascii_digit()
        || next.is_some_and(|ch| ch.is_ascii_lowercase())
}

fn insert_term(raw: &str, terms: &mut BTreeSet<String>) {
    let term = raw.trim_matches('_').to_ascii_lowercase();
    if term.len() >= MIN_QUERY_TERM_LEN
        && term.chars().any(|ch| ch.is_ascii_alphabetic())
        && !QUERY_STOPWORDS.contains(&term.as_str())
    {
        terms.insert(term);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_terms_split_words_snake_case_and_camel_case() {
        let terms = query_terms(Some("PaymentGateway make_target HTTPServer error code x"));

        for expected in ["payment", "gateway", "make", "target", "http", "server"] {
            assert!(terms.contains(expected), "missing {expected}: {terms:?}");
        }
        assert!(!terms.contains("x"));
        assert!(!terms.contains("code"));
        assert!(!terms.contains("error"));
    }
}
