use super::*;
use crate::codemap::CodeReference;
use crate::{HostFile, MemoryCatalogProvider};
use std::path::PathBuf;

// These tests stay in-src (they exercise repomap-internal helpers like
// `analyze_files` / `ReferenceGraph::build` / `page_rank_cancellable`) and so use
// the kernel-resident `MemoryCatalogProvider` rather than `nerve_fs`'s
// `FsCatalogProvider`. The repo-map ranking/graph logic under test is a pure
// function of file CONTENT, so the provider backend is irrelevant here — this
// keeps the tests in-src without widening the kernel's internal API.
fn temp_provider(files: &[(&str, &str)]) -> (MemoryCatalogProvider, CatalogSnapshot) {
    let provider = MemoryCatalogProvider::new(
        files
            .iter()
            .map(|(path, content)| HostFile::new(*path, *content))
            .collect(),
    )
    .expect("provider");
    let snapshot = provider.snapshot().expect("snapshot");
    (provider, snapshot)
}

fn indexed_files(provider: &MemoryCatalogProvider, snapshot: &CatalogSnapshot) -> Vec<IndexedFile> {
    let analyses = analyze_files(provider, snapshot, None).expect("analysis");
    let mut files: Vec<_> = analyses
        .into_iter()
        .filter_map(|analysis| match analysis {
            FileAnalysisResult::Indexed(file) => Some(file),
            _ => None,
        })
        .collect();
    files.sort_by(|left, right| left.path.cmp(&right.path));
    files
}

fn edge_weight(graph: &ReferenceGraph, files: &[IndexedFile], from: &str, to: &str) -> f64 {
    let from_idx = files.iter().position(|file| file.path == from).unwrap();
    let to_idx = files.iter().position(|file| file.path == to).unwrap();
    graph.edges[from_idx]
        .iter()
        .find_map(|(idx, weight)| (*idx == to_idx).then_some(*weight))
        .unwrap_or(0.0)
}

#[test]
fn builds_reference_edges_from_ast_calls_and_type_paths() {
    let (provider, snapshot) = temp_provider(&[
        (
            "target.rs",
            "pub struct Target;\npub fn make_target() -> usize { 1 }\n",
        ),
        (
            "caller.rs",
            "pub fn caller(_value: Target) -> usize { make_target() + make_target() }\n",
        ),
        ("other.rs", "pub fn other() {}\n"),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let graph = ReferenceGraph::build(&files);

    assert!(edge_weight(&graph, &files, "caller.rs", "target.rs") > 0.0);
}

#[test]
fn ignores_identifiers_inside_comments_and_strings() {
    let (provider, snapshot) = temp_provider(&[
        ("target.rs", "pub struct Target;\n"),
        (
            "caller.rs",
            r#"pub fn caller() { let _ = "Target"; /* Target */ } // Target
"#,
        ),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let graph = ReferenceGraph::build(&files);

    assert_eq!(edge_weight(&graph, &files, "caller.rs", "target.rs"), 0.0);
}

#[test]
fn ignores_high_document_frequency_symbols() {
    let (provider, snapshot) = temp_provider(&[
        ("one.rs", "pub fn CommonThing() {}\n"),
        ("two.rs", "pub fn CommonThing() {}\n"),
        ("three.rs", "pub fn CommonThing() {}\n"),
        ("four.rs", "pub fn CommonThing() {}\n"),
        ("caller.rs", "pub fn caller() { CommonThing(); }\n"),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let graph = ReferenceGraph::build(&files);
    let caller_idx = files
        .iter()
        .position(|file| file.path == "caller.rs")
        .unwrap();

    assert!(graph.edges[caller_idx].is_empty());
}

#[test]
fn does_not_create_cross_language_edges_for_same_name() {
    let (provider, snapshot) = temp_provider(&[
        ("shared.js", "export class SharedThing {}\n"),
        ("caller.rs", "pub fn caller() { SharedThing(); }\n"),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let graph = ReferenceGraph::build(&files);

    assert_eq!(edge_weight(&graph, &files, "caller.rs", "shared.js"), 0.0);
}

#[test]
fn js_family_resolves_references_across_ts_and_tsx_display_languages() {
    // Display `language` is the accurate per-extension label, but JS/TS/TSX
    // share one resolution family, so a `.ts` consumer still links to a
    // `.tsx` definition.
    let (provider, snapshot) = temp_provider(&[
        ("widget.tsx", "export class Widget {}\n"),
        (
            "consumer.ts",
            "export function use(w: Widget): Widget { return new Widget(); }\n",
        ),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let language_of = |path: &str| {
        files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .language
            .clone()
    };
    assert_eq!(language_of("widget.tsx"), "tsx");
    assert_eq!(language_of("consumer.ts"), "typescript");

    let graph = ReferenceGraph::build(&files);
    assert!(edge_weight(&graph, &files, "consumer.ts", "widget.tsx") > 0.0);
}

#[test]
fn natural_language_query_terms_personalize_symbol_matches() {
    let (provider, snapshot) = temp_provider(&[
        ("auth.rs", "pub struct AuthService;\n"),
        ("billing.rs", "pub struct PaymentGateway;\n"),
    ]);
    let response = get_repo_map(
        &provider,
        &snapshot,
        &RepoMapRequest {
            query: Some("payment gateway".to_string()),
            seed_paths: vec![],
            max_files: 2,
        },
    )
    .expect("repo map");

    assert_eq!(response.files[0].path, "billing.rs");
    assert_eq!(response.totals.seed_files, 1);
}

#[test]
fn explicit_seed_paths_take_precedence_over_query_terms() {
    let (provider, snapshot) = temp_provider(&[
        ("auth.rs", "pub struct AuthService;\n"),
        ("billing.rs", "pub struct PaymentGateway;\n"),
    ]);
    let response = get_repo_map(
        &provider,
        &snapshot,
        &RepoMapRequest {
            query: Some("payment gateway".to_string()),
            seed_paths: vec![PathBuf::from("auth.rs")],
            max_files: 2,
        },
    )
    .expect("repo map");

    assert_eq!(response.files[0].path, "auth.rs");
    assert_eq!(response.totals.seed_files, 1);
}

#[test]
fn same_language_consumer_reference_ranks_definer_higher() {
    let (provider, snapshot) = temp_provider(&[
        (
            "target.rs",
            "pub struct Target;\npub fn make_target() -> usize { 1 }\n",
        ),
        (
            "caller.rs",
            "pub fn caller(_value: Target) -> usize { make_target() + make_target() }\n",
        ),
        ("isolated.rs", "pub fn isolated() {}\n"),
    ]);
    let response = get_repo_map(
        &provider,
        &snapshot,
        &RepoMapRequest {
            query: Some("make_target".to_string()),
            seed_paths: vec![PathBuf::from("caller.rs")],
            max_files: 3,
        },
    )
    .expect("repo map");

    let target = response
        .files
        .iter()
        .position(|file| file.path == "target.rs")
        .expect("target ranked");
    let caller = response
        .files
        .iter()
        .position(|file| file.path == "caller.rs")
        .expect("caller ranked");
    let target_score: f64 = response.files[target].score.parse().expect("target score");
    let caller_score: f64 = response.files[caller].score.parse().expect("caller score");

    assert!(target < caller);
    assert!(target_score > caller_score);
    assert!(response.totals.edges > 0);
}

#[test]
fn pagerank_prefers_file_referenced_by_multiple_files() {
    let (provider, snapshot) = temp_provider(&[
        ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
        ("caller_one.rs", "pub fn one() -> usize { make_target() }\n"),
        ("caller_two.rs", "pub fn two() -> usize { make_target() }\n"),
    ]);
    let response =
        get_repo_map(&provider, &snapshot, &RepoMapRequest::default()).expect("repo map");

    assert_eq!(response.files[0].path, "target.rs");
}

#[test]
fn python_calls_imports_and_names_build_edges() {
    let (provider, snapshot) = temp_provider(&[
        (
            "target.py",
            "class Target:\n    pass\n\ndef make_target():\n    return Target()\n",
        ),
        (
            "caller.py",
            "from target import Target, make_target\n\ndef caller():\n    value = Target()\n    return make_target()\n",
        ),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let graph = ReferenceGraph::build(&files);

    assert!(edge_weight(&graph, &files, "caller.py", "target.py") > 0.0);
}

#[test]
fn javascript_import_require_calls_and_identifiers_build_edges() {
    let (provider, snapshot) = temp_provider(&[
        ("target.js", "export function makeTarget() { return 1; }\n"),
        (
            "caller.js",
            "import { makeTarget } from './target';\nconst other = require('./target');\nexport function caller() { return makeTarget(); }\n",
        ),
    ]);
    let files = indexed_files(&provider, &snapshot);
    let graph = ReferenceGraph::build(&files);

    assert!(edge_weight(&graph, &files, "caller.js", "target.js") > 0.0);
}

#[test]
fn embedded_markdown_imports_do_not_resolve_relative_to_host_doc() {
    let files = vec![
        IndexedFile {
            path: "README.md".to_string(),
            display_path: "README.md".to_string(),
            abs_path: PathBuf::from("README.md"),
            language: "markdown".to_string(),
            symbols: Vec::new(),
            references: vec![
                CodeReference::new("import", "target", 2)
                    .with_language("javascript")
                    .with_import_path("./target"),
            ],
            query_match: false,
        },
        IndexedFile {
            path: "target.js".to_string(),
            display_path: "target.js".to_string(),
            abs_path: PathBuf::from("target.js"),
            language: "javascript".to_string(),
            symbols: Vec::new(),
            references: Vec::new(),
            query_match: false,
        },
    ];
    let graph = ReferenceGraph::build(&files);

    assert_eq!(edge_weight(&graph, &files, "README.md", "target.js"), 0.0);
}

#[test]
fn personalized_pagerank_biases_seed_files() {
    let (provider, snapshot) = temp_provider(&[
        ("target.rs", "pub fn make_target() -> usize { 1 }\n"),
        ("caller.rs", "pub fn caller() -> usize { make_target() }\n"),
        ("isolated.rs", "pub fn isolated() {}\n"),
    ]);
    let response = get_repo_map(
        &provider,
        &snapshot,
        &RepoMapRequest {
            query: None,
            seed_paths: vec![PathBuf::from("isolated.rs")],
            max_files: 3,
        },
    )
    .expect("repo map");

    assert_eq!(response.files[0].path, "isolated.rs");
    assert_eq!(response.totals.seed_files, 1);
}

#[test]
fn pre_cancelled_repo_map_returns_cancelled() {
    let (provider, snapshot) = temp_provider(&[("lib.rs", "pub fn alpha() {}\n")]);
    let token = CancelToken::new();
    token.cancel();

    let snapshot = std::sync::Arc::new(snapshot);
    let err = get_repo_map_cancellable(&provider, &snapshot, &RepoMapRequest::default(), &token)
        .expect_err("repo-map should cancel");
    assert!(matches!(err, NerveError::Cancelled));
}

#[test]
fn repo_map_cancel_after_n_checks_is_deterministic() {
    let (provider, snapshot) = temp_provider(&[
        ("target.rs", "pub struct Target;\n"),
        ("caller.rs", "pub fn caller() { let _ = Target; }\n"),
    ]);
    let token = CancelToken::cancel_after_checks(3);

    let snapshot = std::sync::Arc::new(snapshot);
    let err = get_repo_map_cancellable(&provider, &snapshot, &RepoMapRequest::default(), &token)
        .expect_err("repo-map should cancel after injected check count");
    assert!(matches!(err, NerveError::Cancelled));
}

#[test]
fn pagerank_checks_cancel_each_iteration() {
    let edges = vec![vec![(1, 1.0)], vec![(0, 1.0)]];
    let personalization = vec![0.5, 0.5];
    let token = CancelToken::cancel_after_checks(1);

    let err = page_rank_cancellable(&edges, &personalization, &token)
        .expect_err("pagerank should cancel on injected iteration check");
    assert!(matches!(err, NerveError::Cancelled));
}
