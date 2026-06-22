use nerve_core::{
    BuildContextRequest, CatalogProvider, FsCatalogProvider, RootPolicy, ScanOptions, build_context,
};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
use nerve_core::{
    CancelToken, HostFile, MemoryCatalogProvider, NerveError, SemanticSearchRequest,
    semantic::{EmbeddingBackend, SemanticIndex, SemanticIndexConfig},
};
#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn provider_for(dir: &Path) -> (FsCatalogProvider, nerve_core::CatalogSnapshot) {
    let provider = FsCatalogProvider::new(
        RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
        ScanOptions::default(),
    );
    let snapshot = provider.snapshot().expect("snapshot");
    (provider, snapshot)
}

#[test]
fn ranking_prefers_content_match_over_path_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("aaa_path_needle.txt"), "nothing\n").expect("write");
    fs::write(dir.path().join("zzz.txt"), "needle\n").expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle".to_string(),
            token_budget: 120,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert_eq!(response.manifest.included[0].path, "zzz.txt");
}

#[test]
fn budget_downgrades_rust_file_to_codemap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body = format!(
        "{}\n{}",
        "pub fn needle_target() {}",
        "// filler text to make the full file too expensive\n".repeat(120)
    );
    fs::write(dir.path().join("lib.rs"), body).expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle_target".to_string(),
            token_budget: 120,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert_eq!(response.manifest.included[0].mode, "codemap_only");
    assert!(response.manifest.token_used <= response.manifest.token_budget);
}

#[test]
fn huge_text_file_uses_search_slices() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut text = "filler\n".repeat(120);
    text.push_str("needle line\n");
    text.push_str(&"tail\n".repeat(120));
    fs::write(dir.path().join("huge.txt"), text).expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle".to_string(),
            token_budget: 120,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert_eq!(response.manifest.included[0].mode, "slices");
    assert!(response.context.contains("needle line"));
}

#[test]
fn seed_paths_include_the_seeded_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("hit.rs"), "pub fn needle() {}\n").expect("write");
    fs::write(dir.path().join("seed.rs"), "pub fn other_thing() {}\n").expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle".to_string(),
            token_budget: 400,
            max_files: Some(5),
            seed_paths: vec![PathBuf::from("seed.rs")],
        },
    )
    .expect("build context");

    let included: Vec<&str> = response
        .manifest
        .included
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert!(
        included.contains(&"seed.rs"),
        "seeded file should be included: {included:?}"
    );
}

#[test]
fn reference_expansion_adds_codemap_only_defining_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("seed.js"),
        "function seedMarker() { return new ReferencedThing(); }\n",
    )
    .expect("write");
    fs::write(dir.path().join("types.js"), "class ReferencedThing {}\n").expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "seedMarker".to_string(),
            token_budget: 500,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    let expanded = response
        .manifest
        .included
        .iter()
        .find(|file| file.path == "types.js")
        .expect("referenced definition file included");
    assert_eq!(expanded.mode, "codemap_only");
    assert!(
        response
            .manifest
            .excluded
            .iter()
            .all(|file| file.path != "types.js")
    );
    assert!(response.manifest.token_used <= response.manifest.token_budget);
}

#[test]
fn reference_expansion_uses_reference_only_markdown_fence() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("README.md"),
        "# Example\n\n```javascript\nnew ReferencedThing();\n```\n",
    )
    .expect("write readme");
    fs::write(dir.path().join("types.js"), "class ReferencedThing {}\n").expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "Example".to_string(),
            token_budget: 700,
            max_files: Some(1),
            seed_paths: vec![PathBuf::from("README.md")],
        },
    )
    .expect("build context");

    let expanded = response
        .manifest
        .included
        .iter()
        .find(|file| file.path == "types.js")
        .expect("referenced definition file included");
    assert_eq!(expanded.mode, "codemap_only");
}

#[test]
fn reference_expansion_skips_when_budget_exhausted() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("seed.js"),
        "function seedMarker() { return new HugeReferencedThing(); }\n",
    )
    .expect("write");
    let extra_types = (0..80)
        .map(|idx| format!("class ExtraReferencedThing{idx} {{}}\n"))
        .collect::<String>();
    fs::write(
        dir.path().join("types.js"),
        format!("class HugeReferencedThing {{}}\n{extra_types}"),
    )
    .expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "seedMarker".to_string(),
            token_budget: 120,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert!(
        response
            .manifest
            .included
            .iter()
            .all(|file| file.path != "types.js")
    );
    assert!(response
        .manifest
        .excluded
        .iter()
        .any(|file| file.path == "types.js" && file.reason == "reference_expansion_over_budget"));
    assert!(response.manifest.token_used <= response.manifest.token_budget);
}

#[test]
fn tiny_budget_returns_no_files_and_preserves_selection() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("file.txt"), "needle\n").expect("write");
    let (provider, snapshot) = provider_for(dir.path());
    let before = provider.selection();

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle".to_string(),
            token_budget: 0,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert!(response.manifest.included.is_empty());
    assert_eq!(response.manifest.token_used, 0);
    assert_eq!(provider.selection(), before);
}

#[test]
fn over_budget_candidate_does_not_stop_later_small_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let large = format!("needle {}\n", "very_large_token ".repeat(300));
    fs::write(dir.path().join("aaa_large.txt"), large).expect("write");
    fs::write(dir.path().join("zzz_small.txt"), "needle\n").expect("write");
    let (provider, snapshot) = provider_for(dir.path());

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle".to_string(),
            token_budget: 90,
            max_files: Some(2),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert!(
        response
            .manifest
            .included
            .iter()
            .any(|file| file.path == "zzz_small.txt")
    );
    assert!(
        response
            .manifest
            .excluded
            .iter()
            .any(|file| file.path == "aaa_large.txt" && file.reason == "over_budget")
    );
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
#[test]
fn warm_semantic_index_can_seed_build_context_ranking() {
    let provider = MemoryCatalogProvider::new(vec![
        HostFile::new(
            "alpha.rs",
            b"pub fn parse_config() { validate_config(); }".to_vec(),
        ),
        HostFile::new(
            "beta.rs",
            b"pub fn render_view() { draw_button(); }".to_vec(),
        ),
    ])
    .expect("provider");
    let snapshot = provider.snapshot().expect("snapshot");
    let index = Arc::new(SemanticIndex::mock());
    index
        .search(
            &provider,
            &snapshot,
            &SemanticSearchRequest {
                query: "config validation".into(),
                rerank: false,
                ..SemanticSearchRequest::default()
            },
            &CancelToken::never(),
        )
        .expect("warm semantic index");
    provider.set_semantic_index(Some(index));

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "config validation".to_string(),
            token_budget: 400,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert_eq!(response.manifest.included[0].path, "alpha.rs");
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
#[test]
fn cold_semantic_index_falls_back_without_embedding() {
    let provider = MemoryCatalogProvider::new(vec![
        HostFile::new("needle.txt", b"needle\n".to_vec()),
        HostFile::new("other.txt", b"other\n".to_vec()),
    ])
    .expect("provider");
    let snapshot = provider.snapshot().expect("snapshot");
    let backend = Arc::new(CountingEmbeddingBackend::default());
    provider.set_semantic_index(Some(Arc::new(SemanticIndex::new(
        SemanticIndexConfig::default(),
        backend.clone(),
        None,
    ))));

    let response = build_context(
        &provider,
        &snapshot,
        &BuildContextRequest {
            query: "needle".to_string(),
            token_budget: 200,
            max_files: Some(1),
            seed_paths: Vec::new(),
        },
    )
    .expect("build context");

    assert_eq!(response.manifest.included[0].path, "needle.txt");
    assert_eq!(backend.calls(), 0);
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
#[derive(Default)]
struct CountingEmbeddingBackend {
    calls: AtomicUsize,
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl CountingEmbeddingBackend {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl EmbeddingBackend for CountingEmbeddingBackend {
    fn dimension(&self) -> usize {
        32
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, NerveError> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(vec![vec![0.0; self.dimension()]; texts.len()])
    }

    fn embed_query(&self, _query: &str) -> Result<Vec<f32>, NerveError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![0.0; self.dimension()])
    }
}
