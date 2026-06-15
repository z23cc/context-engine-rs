use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ctx_core::{
    CatalogProvider, FsCatalogProvider, RootPolicy, ScanOptions, SearchMode, SearchRequest,
    handle_tool_call, search_snapshot,
};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
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
        regex: false,
        max_results: 10_000,
        context_lines: 1,
        max_content_files: FILE_COUNT + 128,
        max_content_bytes: 512 * 1024 * 1024,
        whole_word: false,
    }
}

fn path_request() -> SearchRequest {
    SearchRequest {
        pattern: "module_4/file".to_string(),
        mode: SearchMode::Path,
        regex: false,
        max_results: 10_000,
        context_lines: 0,
        max_content_files: 0,
        max_content_bytes: 0,
        whole_word: false,
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

criterion_group!(
    benches,
    bench_catalog_scan,
    bench_content_search,
    bench_path_search,
    bench_repeated_tool_search
);
criterion_main!(benches);
