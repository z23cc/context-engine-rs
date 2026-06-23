use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use nerve_core::{
    CatalogProvider, FsCatalogProvider, RootPolicy, ScanOptions, SearchMode, SearchRequest,
    handle_tool_call, search_snapshot,
};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::TempDir;

const FILE_COUNT: usize = 4_096;

struct Corpus {
    _temp: TempDir,
    root: PathBuf,
    files: usize,
    bytes: u64,
}

fn build_corpus() -> Corpus {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("synthetic-corpus");
    fs::create_dir(&root).expect("create corpus root");

    let mut total_bytes = 0_u64;
    for idx in 0..FILE_COUNT {
        let dir = root.join(format!("module_{:02}", idx % 64));
        fs::create_dir_all(&dir).expect("create module dir");
        let ext = match idx % 4 {
            0 => "rs",
            1 => "md",
            2 => "txt",
            _ => "json",
        };
        let path = dir.join(format!("file_{idx:05}.{ext}"));
        let mut file = fs::File::create(&path).expect("create file");
        let lines = 12 + (idx % 97);
        for line in 0..lines {
            if line == idx % lines && idx % 3 == 0 {
                writeln!(file, "pub fn bench_needle_{idx:05}_{line:03}() {{ /* deterministic content search needle */ }}").expect("write needle");
            } else {
                writeln!(
                    file,
                    "file={idx:05} line={line:03} module={} alpha beta gamma delta epsilon",
                    idx % 64
                )
                .expect("write filler");
            }
        }
        total_bytes += fs::metadata(&path).expect("metadata").len();
    }

    Corpus {
        _temp: temp,
        root,
        files: FILE_COUNT,
        bytes: total_bytes,
    }
}

fn provider_for(root: &Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![root.to_path_buf()]).expect("root policy"),
        ScanOptions {
            max_entries: FILE_COUNT + 128,
            ..ScanOptions::default()
        },
    )
}

fn content_request() -> SearchRequest {
    SearchRequest {
        pattern: "needle".to_string(),
        mode: SearchMode::Content,
        max_results: 10_000,
        context_lines: 1,
        max_content_files: FILE_COUNT + 128,
        max_content_bytes: 512 * 1024 * 1024,
        ..SearchRequest::default()
    }
}

fn path_request() -> SearchRequest {
    SearchRequest {
        pattern: "module_4/file".to_string(),
        mode: SearchMode::Path,
        max_results: 10_000,
        context_lines: 0,
        max_content_files: 0,
        max_content_bytes: 0,
        ..SearchRequest::default()
    }
}

fn bench_catalog_scan(c: &mut Criterion) {
    let corpus = build_corpus();
    let mut group = c.benchmark_group("catalog_scan");
    group.throughput(Throughput::Elements(corpus.files as u64));
    group.bench_with_input(
        BenchmarkId::new("snapshot", corpus.files),
        &corpus.root,
        |b, root| {
            b.iter(|| {
                let provider = provider_for(root);
                let snapshot = provider.snapshot().expect("snapshot");
                assert_eq!(snapshot.entries.len(), FILE_COUNT);
                snapshot
            });
        },
    );
    group.finish();
}

fn bench_content_search(c: &mut Criterion) {
    let corpus = build_corpus();
    let provider = provider_for(&corpus.root);
    let snapshot = provider.snapshot().expect("snapshot");
    let request = content_request();

    let mut group = c.benchmark_group("content_search");
    group.throughput(Throughput::Bytes(corpus.bytes));
    group.bench_function(BenchmarkId::new("literal", corpus.files), |b| {
        b.iter(|| {
            let response = search_snapshot(&provider, &snapshot, &request).expect("search");
            assert_eq!(response.totals.content_files_scanned, FILE_COUNT);
            assert!(response.totals.content_matches > 1_000);
            response
        });
    });
    group.finish();
}

fn bench_repeated_tool_search(c: &mut Criterion) {
    const REQUESTS_PER_ITERATION: usize = 16;

    let corpus = build_corpus();
    let provider = provider_for(&corpus.root);
    let params = json!({
        "name": "file_search",
        "arguments": {
            "pattern": "module_4/file",
            "mode": "path",
            "max_results": 10_000
        }
    });

    let mut group = c.benchmark_group("repeated_tool_search");
    group.throughput(Throughput::Elements(
        (corpus.files * REQUESTS_PER_ITERATION) as u64,
    ));
    group.bench_function(
        BenchmarkId::new("uncached_snapshot_each_request", corpus.files),
        |b| {
            b.iter(|| {
                let mut responses = 0usize;
                for _ in 0..REQUESTS_PER_ITERATION {
                    provider.invalidate();
                    let response = handle_tool_call(&provider, &params).expect("tool call");
                    assert!(response.get("structuredContent").is_some());
                    responses += 1;
                }
                responses
            });
        },
    );
    group.bench_function(
        BenchmarkId::new("cached_snapshot_reused", corpus.files),
        |b| {
            b.iter(|| {
                provider.invalidate();
                let mut responses = 0usize;
                for _ in 0..REQUESTS_PER_ITERATION {
                    let response = handle_tool_call(&provider, &params).expect("tool call");
                    assert!(response.get("structuredContent").is_some());
                    responses += 1;
                }
                responses
            });
        },
    );
    group.finish();
}

fn bench_path_search(c: &mut Criterion) {
    let corpus = build_corpus();
    let provider = provider_for(&corpus.root);
    let snapshot = provider.snapshot().expect("snapshot");
    let request = path_request();

    let mut group = c.benchmark_group("path_search");
    group.throughput(Throughput::Elements(corpus.files as u64));
    group.bench_function(BenchmarkId::new("literal", corpus.files), |b| {
        b.iter(|| {
            let response = search_snapshot(&provider, &snapshot, &request).expect("search");
            assert!(response.totals.path_matches > 0);
            response
        });
    });
    group.finish();
}

/// A provider whose snapshot cache effectively never expires, so warm benches
/// measure a *pure* memo hit (stable snapshot `Arc`) without a mid-measurement
/// TTL rebuild.
fn warm_provider_for(root: &Path) -> FsCatalogProvider {
    FsCatalogProvider::new(
        RootPolicy::new(vec![root.to_path_buf()]).expect("root policy"),
        ScanOptions {
            max_entries: FILE_COUNT + 128,
            snapshot_cache_ttl: Duration::from_secs(3_600),
        },
    )
}

/// Quantify the CodeGraph T0 memoization (PR1/PR1b/PR1c) in the two scenarios a
/// cockpit actually hits:
///   * **cold** — a fresh snapshot every call (`invalidate()` per iteration): catalog
///     rescan + cross-file re-derivation. NOTE since T1a (the codemap parse cache is
///     retained across invalidate) this path no longer re-parses unchanged files, so
///     it measures the realistic post-edit / post-TTL cost (~50–115 ms here), not the
///     one-time full-parse cold start (~1.5 s on an empty cache, e.g. process start).
///   * **warm** — repeated queries against a stable cached snapshot: the shared
///     index / reference graph / definition index are memoized on the snapshot
///     `Arc`, so derivation collapses to an O(1) lookup.
///
/// Together they gate further work (on-disk persistence would only help the one-time
/// full-parse cold start) and show what the shipped memoization delivers steady-state.
///
/// Caveat: the synthetic corpus has definitions but almost no cross-file
/// references, so warm `find_references` mainly reflects the memo eliminating the
/// rebuild (its residual reference scan is near-zero here); warm `get_repo_map`
/// still pays PageRank (not memoized) over the cached graph; warm `detect_changes`
/// is bounded by the one changed file, not repo size.
fn bench_code_graph(c: &mut Criterion) {
    let corpus = build_corpus();
    let cold_provider = provider_for(&corpus.root);
    let warm_provider = warm_provider_for(&corpus.root);

    let find_references = json!({
        "name": "find_references",
        "arguments": { "symbol": "bench_needle_00000_000", "max_results": 1_000 }
    });
    let repo_map = json!({ "name": "get_repo_map", "arguments": { "max_files": 50 } });
    let detect_changes = json!({
        "name": "detect_changes",
        "arguments": { "diff":
            "--- a/module_00/file_00000.rs\n+++ b/module_00/file_00000.rs\n@@ -1 +1 @@\n-old\n+new\n"
        }
    });

    // Prime the warm provider's snapshot + every memo once, so each measured warm
    // iteration is a pure hit.
    for params in [&find_references, &repo_map, &detect_changes] {
        handle_tool_call(&warm_provider, params).expect("prime warm memo");
    }

    let mut group = c.benchmark_group("code_graph");
    group.throughput(Throughput::Elements(corpus.files as u64));

    let mut cold = |label: &str, params: &serde_json::Value| {
        group.bench_function(BenchmarkId::new(label, corpus.files), |b| {
            b.iter(|| {
                cold_provider.invalidate();
                handle_tool_call(&cold_provider, params).expect("cold tool call")
            });
        });
    };
    cold("find_references_cold", &find_references);
    cold("get_repo_map_cold", &repo_map);
    cold("detect_changes_cold", &detect_changes);

    let mut warm = |label: &str, params: &serde_json::Value| {
        group.bench_function(BenchmarkId::new(label, corpus.files), |b| {
            b.iter(|| handle_tool_call(&warm_provider, params).expect("warm tool call"));
        });
    };
    warm("find_references_warm", &find_references);
    warm("get_repo_map_warm", &repo_map);
    warm("detect_changes_warm", &detect_changes);

    group.finish();
}

criterion_group!(
    benches,
    bench_catalog_scan,
    bench_content_search,
    bench_path_search,
    bench_repeated_tool_search,
    bench_code_graph
);
criterion_main!(benches);
