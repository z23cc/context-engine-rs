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
    // Only the PURE (no-provider) search tests remain in-src: they exercise
    // private path-scoring internals (`path_score`, `PathQuery`, ...) reachable
    // here via `use super::*`. The provider-dependent search tests moved to
    // `tests/relocated_search.rs` (they need `nerve_fs::FsCatalogProvider`, which
    // the `dev-dependencies` back-edge forbids in an in-src `#[cfg(test)]` module).
    use super::*;
    use crate::ranking::{EntryFilter, EntryFilterConfig};

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
    fn glob_filters_preserve_search_semantics() {
        let bare = EntryFilter::from_config(&EntryFilterConfig {
            include: vec!["*.rs".into()],
            ..EntryFilterConfig::default()
        })
        .unwrap();
        assert!(bare.accepts("a.rs"));
        assert!(bare.accepts("src/deep/b.rs"));
        assert!(!bare.accepts("a.txt"));

        let scoped = EntryFilter::from_config(&EntryFilterConfig {
            include: vec!["src/*.rs".into()],
            ..EntryFilterConfig::default()
        })
        .unwrap();
        assert!(scoped.accepts("src/a.rs"));
        assert!(!scoped.accepts("src/deep/a.rs"));

        let deep = EntryFilter::from_config(&EntryFilterConfig {
            include: vec!["src/**/*.rs".into()],
            ..EntryFilterConfig::default()
        })
        .unwrap();
        assert!(deep.accepts("src/deep/a.rs"));
        assert!(deep.accepts("src/a.rs"));
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
}
