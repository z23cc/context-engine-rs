//! Path and content search over immutable catalog snapshots.

use crate::{
    cancel::CancelToken,
    models::*,
    port::CatalogProvider,
    ranking::{ContentRankingQuery, EntryFilter, FileRankingStats, content_file_scores, is_binary},
    snapshot::CatalogSnapshot,
};
use aho_corasick::{AhoCorasick, AhoCorasickBuilder};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
};
mod api;
mod content;
mod matcher;
mod path;

pub use api::{search_snapshot, search_snapshot_cancellable};

use content::*;
use matcher::*;
use path::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ranking::glob_to_regex;
    use crate::{FsCatalogProvider, RootPolicy, catalog::ScanOptions};
    use std::fs;

    fn request(pattern: &str, mode: SearchMode, max_results: usize) -> SearchRequest {
        SearchRequest {
            pattern: pattern.to_string(),
            mode,
            max_results,
            context_lines: 0,
            ..SearchRequest::default()
        }
    }

    fn filtered(pattern: &str, f: impl FnOnce(&mut SearchRequest)) -> SearchRequest {
        let mut req = SearchRequest {
            pattern: pattern.to_string(),
            mode: SearchMode::Content,
            ..SearchRequest::default()
        };
        f(&mut req);
        req
    }

    #[test]
    fn glob_to_regex_segment_and_recursive_semantics() {
        // bare glob with no slash matches basename at any depth
        let any = Regex::new(&glob_to_regex("*.rs")).unwrap();
        assert!(any.is_match("a.rs"));
        assert!(any.is_match("src/deep/b.rs"));
        assert!(!any.is_match("a.txt"));
        // anchored dir glob: * stays within a segment
        let scoped = Regex::new(&glob_to_regex("src/*.rs")).unwrap();
        assert!(scoped.is_match("src/a.rs"));
        assert!(!scoped.is_match("src/deep/a.rs"));
        // ** crosses segments
        let deep = Regex::new(&glob_to_regex("src/**/*.rs")).unwrap();
        assert!(deep.is_match("src/deep/a.rs"));
        assert!(deep.is_match("src/a.rs"));
    }

    #[test]
    fn extension_and_glob_filters_narrow_results() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "needle\n").expect("w");
        fs::write(dir.path().join("b.txt"), "needle\n").expect("w");
        std::fs::create_dir(dir.path().join("vendor")).expect("mkdir");
        fs::write(dir.path().join("vendor/c.rs"), "needle\n").expect("w");
        let (provider, snapshot) = provider_for(dir.path());

        let by_ext = search_snapshot(
            &provider,
            &snapshot,
            &filtered("needle", |r| r.extensions = vec!["rs".into()]),
        )
        .expect("ext");
        assert!(
            by_ext
                .content_matches
                .iter()
                .all(|m| m.path.ends_with(".rs"))
        );
        assert_eq!(by_ext.content_matches.len(), 2);

        let excluded = search_snapshot(
            &provider,
            &snapshot,
            &filtered("needle", |r| {
                r.extensions = vec!["rs".into()];
                r.exclude = vec!["vendor/**".into()];
            }),
        )
        .expect("exclude");
        assert_eq!(excluded.content_matches.len(), 1);
        assert_eq!(excluded.content_matches[0].path, "a.rs");
    }

    #[test]
    fn output_mode_files_and_count_collapse_to_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "needle\nneedle\n").expect("w");
        fs::write(dir.path().join("b.rs"), "needle\n").expect("w");
        let (provider, snapshot) = provider_for(dir.path());

        let files = search_snapshot(
            &provider,
            &snapshot,
            &filtered("needle", |r| r.output_mode = OutputMode::FilesWithMatches),
        )
        .expect("files");
        assert!(files.content_matches.is_empty());
        assert_eq!(files.match_files.len(), 2);
        // ordered by count desc: a.rs (2) before b.rs (1)
        assert_eq!(files.match_files[0].path, "a.rs");
        assert_eq!(files.match_files[0].count, 2);
    }

    #[test]
    fn asymmetric_context_before_after() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "l1\nl2\nMATCH\nl4\nl5\n").expect("w");
        let (provider, snapshot) = provider_for(dir.path());
        let resp = search_snapshot(
            &provider,
            &snapshot,
            &filtered("MATCH", |r| {
                r.context_before = Some(2);
                r.context_after = Some(0);
            }),
        )
        .expect("ctx");
        let ctx = &resp.content_matches[0].context;
        // lines 1,2,3 (two before + the match), none after
        assert_eq!(ctx.first().unwrap().line, 1);
        assert_eq!(ctx.last().unwrap().line, 3);
    }

    fn provider_for(dir: &Path) -> (FsCatalogProvider, CatalogSnapshot) {
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        (provider, snapshot)
    }

    fn score_path(
        path: &str,
        pattern: &str,
        case_sensitive: bool,
        whole_word: bool,
    ) -> Option<i64> {
        let fuzzy_pattern = Pattern::new(
            pattern,
            nucleo_case_matching(case_sensitive),
            Normalization::Never,
            AtomKind::Fuzzy,
        );
        let query = PathQuery {
            pattern,
            regex: None,
            case_sensitive,
            whole_word,
            fuzzy_pattern: Some(&fuzzy_pattern),
        };
        let mut state = PathMatcherState::new(case_sensitive);
        path_score(path, query, &mut state)
    }

    #[test]
    fn finds_path_and_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("src")).expect("src");
        fs::write(dir.path().join("src/lib.rs"), "pub fn needle() {}\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Both, 10),
        )
        .expect("search");
        assert_eq!(response.generation, snapshot.generation);
        assert_eq!(response.totals.content_matches, 1);
        assert_eq!(response.content_matches.len(), 1);
    }

    #[test]
    fn returns_per_bucket_top_k_after_independent_ranking() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("zzz_needles.txt"), "needle late path\n").expect("write zzz");
        fs::write(dir.path().join("needle.rs"), "pub fn unrelated() {}\n").expect("write needle");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Both, 1),
        )
        .expect("search");
        assert_eq!(
            response.totals.path_matches + response.totals.content_matches,
            3
        );
        assert_eq!(response.totals.omitted, 1);
        assert_eq!(response.path_matches.len(), 1);
        assert_eq!(response.path_matches[0].path, "needle.rs");
        assert_eq!(response.content_matches.len(), 1);
        assert_eq!(response.content_matches[0].path, "zzz_needles.txt");
    }

    #[test]
    fn nucleo_path_scoring_prefers_boundaries_and_contiguous_runs() {
        let boundary = score_path("src/FooBar.rs", "fb", false, false).expect("boundary");
        let word_middle = score_path("src/afobb.rs", "fb", false, false).expect("middle");
        assert!(boundary > word_middle);

        let contiguous = score_path("src/foo_bar.rs", "foo", false, false).expect("contiguous");
        let jumping = score_path("src/f_a_o_o.rs", "foo", false, false).expect("jumping");
        assert!(contiguous > jumping);

        let substring = score_path("src/foo.rs", "foo", false, false).expect("substring");
        assert!(substring > contiguous);
    }

    #[test]
    fn path_results_are_sorted_by_nucleo_score() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("src");
        fs::write(dir.path().join("src/FooBar.rs"), "").expect("boundary");
        fs::write(dir.path().join("src/afobb.rs"), "").expect("middle");
        fs::write(dir.path().join("src/f_a_o_o.rs"), "").expect("jumping");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(&provider, &snapshot, &request("fb", SearchMode::Path, 10))
            .expect("search");
        assert_eq!(response.path_matches[0].path, "src/FooBar.rs");
    }

    #[test]
    fn bm25_multi_term_content_ranking_prefers_relevant_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("focused.txt"),
            "alpha beta alpha beta alpha beta\n",
        )
        .expect("focused");
        fs::write(
            dir.path().join("diluted.txt"),
            format!("alpha beta {}\n", "filler ".repeat(200)),
        )
        .expect("diluted");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("alpha beta", SearchMode::Content, 10),
        )
        .expect("search");
        assert_eq!(response.content_matches[0].path, "focused.txt");
        assert!(response.content_matches[0].score > response.content_matches[1].score);
    }

    #[test]
    fn single_term_content_ranking_uses_tf_saturation_and_length_norm() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("dense.txt"), "needle needle needle\n").expect("dense");
        fs::write(
            dir.path().join("sparse.txt"),
            format!("needle {}\n", "filler ".repeat(200)),
        )
        .expect("sparse");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
        )
        .expect("search");
        assert_eq!(response.content_matches[0].path, "dense.txt");
        assert!(response.content_matches[0].score > response.content_matches[1].score);
    }

    #[test]
    fn content_results_are_per_file_capped_and_round_robin_interleaved() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("noisy.txt"), "needle\n".repeat(30)).expect("noisy");
        fs::write(dir.path().join("broad_a.txt"), "needle\n".repeat(3)).expect("broad a");
        fs::write(dir.path().join("broad_b.txt"), "needle\n".repeat(3)).expect("broad b");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 9),
        )
        .expect("search");

        let paths: Vec<_> = response
            .content_matches
            .iter()
            .map(|hit| hit.path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec![
                "noisy.txt",
                "broad_a.txt",
                "broad_b.txt",
                "noisy.txt",
                "broad_a.txt",
                "broad_b.txt",
                "noisy.txt",
                "broad_a.txt",
                "broad_b.txt",
            ]
        );
        assert_eq!(response.totals.content_matches, 36);
        assert_eq!(response.totals.omitted, 27);
        assert!(response.totals.budget.exhausted);
    }

    #[test]
    fn regex_content_ranking_falls_back_to_tf_density() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("dense.txt"), "needle\nneedle\nneedle\n").expect("dense");
        fs::write(
            dir.path().join("sparse.txt"),
            format!("needle\n{}\n", "filler\n".repeat(200)),
        )
        .expect("sparse");
        let (provider, snapshot) = provider_for(dir.path());
        let mut req = request(r"need(le)?", SearchMode::Content, 10);
        req.regex = true;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        let dense_score = response
            .content_matches
            .iter()
            .find(|hit| hit.path == "dense.txt")
            .expect("dense hit")
            .score;
        let sparse_score = response
            .content_matches
            .iter()
            .find(|hit| hit.path == "sparse.txt")
            .expect("sparse hit")
            .score;
        assert!(dense_score > sparse_score);
    }

    #[test]
    fn smart_case_literal_search_is_insensitive_until_pattern_has_uppercase() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("case.txt"), "Needle\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());

        let insensitive = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
        )
        .expect("insensitive search");
        assert_eq!(insensitive.totals.content_matches, 1);

        let sensitive = search_snapshot(
            &provider,
            &snapshot,
            &request("NeedleX", SearchMode::Content, 10),
        )
        .expect("sensitive search");
        assert_eq!(sensitive.totals.content_matches, 0);
    }

    #[test]
    fn smart_case_regex_search_is_insensitive_until_pattern_has_uppercase() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("case.txt"), "Needle\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());

        let mut insensitive = request("needle", SearchMode::Content, 10);
        insensitive.regex = true;
        let insensitive =
            search_snapshot(&provider, &snapshot, &insensitive).expect("regex search");
        assert_eq!(insensitive.totals.content_matches, 1);

        let mut sensitive = request("needle[A-Z]", SearchMode::Content, 10);
        sensitive.regex = true;
        let sensitive = search_snapshot(&provider, &snapshot, &sensitive).expect("regex search");
        assert_eq!(sensitive.totals.content_matches, 0);
    }

    #[test]
    fn binary_files_are_skipped_for_content_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("binary.bin"), b"needle\0needle\n").expect("write binary");
        fs::write(dir.path().join("text.txt"), "needle\n").expect("write text");
        let (provider, snapshot) = provider_for(dir.path());
        let response = search_snapshot(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
        )
        .expect("search");

        assert_eq!(response.totals.content_files_scanned, 2);
        assert_eq!(response.totals.binary_files_skipped, 1);
        assert_eq!(response.totals.content_matches, 1);
        assert_eq!(response.content_matches[0].path, "text.txt");
        assert_eq!(response.diagnostics.len(), 1);
        assert_eq!(
            response.diagnostics[0].path,
            Some(PathBuf::from("binary.bin"))
        );
    }

    #[test]
    fn whole_word_literal_filters_subword_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("words.txt"), "needle needless\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let mut req = request("needle", SearchMode::Content, 10);
        req.whole_word = true;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.totals.content_matches, 1);
        assert_eq!(response.content_matches[0].column, 1);
    }

    #[test]
    fn enforces_content_file_limit_with_lower_bound_totals() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "needle one\n").expect("write a");
        fs::write(dir.path().join("b.txt"), "needle two\n").expect("write b");
        let (provider, snapshot) = provider_for(dir.path());
        let mut req = request("needle", SearchMode::Content, 10);
        req.max_content_files = 1;
        req.max_content_bytes = 64 * 1024;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.totals.content_files_scanned, 1);
        assert_eq!(response.content_matches.len(), 1);
        assert!(response.totals.totals_are_lower_bound);
        assert!(response.totals.budget.exhausted);
    }

    #[test]
    fn enforces_content_byte_limit_before_reading_next_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "needle one\n").expect("write a");
        fs::write(dir.path().join("b.txt"), "needle two\n").expect("write b");
        let (provider, snapshot) = provider_for(dir.path());
        let first_size = snapshot
            .entries
            .iter()
            .find(|entry| entry.rel_path == "a.txt")
            .expect("a entry")
            .size;
        let mut req = request("needle", SearchMode::Content, 10);
        req.max_content_files = 10;
        req.max_content_bytes = first_size;
        let response = search_snapshot(&provider, &snapshot, &req).expect("search");
        assert_eq!(response.totals.content_files_scanned, 1);
        assert_eq!(response.totals.content_bytes_scanned, first_size);
        assert!(response.totals.totals_are_lower_bound);
        assert!(response.totals.budget.exhausted);
    }

    #[test]
    fn pre_cancelled_content_search_returns_cancelled() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("text.txt"), "needle\n").expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let token = CancelToken::new();
        token.cancel();

        let err = search_snapshot_cancellable(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
            &token,
        )
        .expect_err("search should cancel");
        assert!(matches!(err, CtxError::Cancelled));
    }

    #[test]
    fn content_search_cancel_after_n_checks_is_deterministic() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("text.txt"),
            "needle\nneedle\nneedle\nneedle\n",
        )
        .expect("write");
        let (provider, snapshot) = provider_for(dir.path());
        let token = CancelToken::cancel_after_checks(5);

        let err = search_snapshot_cancellable(
            &provider,
            &snapshot,
            &request("needle", SearchMode::Content, 10),
            &token,
        )
        .expect_err("content search should cancel after injected check count");
        assert!(matches!(err, CtxError::Cancelled));
    }
}
