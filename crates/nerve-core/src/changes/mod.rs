//! `detect_changes` — map a unified diff to the symbols it touches.
//!
//! Given a unified diff as **text** (the agent or host pipes in `git diff`
//! output — no VCS is invoked here, keeping the determinism boundary intact), this
//! reports, per changed file, the symbols whose tree-sitter block span intersects
//! a changed line. It is the "what did this change touch" companion to
//! `analyze_impact` ("who depends on this symbol"): an agent runs `detect_changes`
//! on its own diff, then optionally feeds the affected symbols to `analyze_impact`
//! for the blast radius.
//!
//! Deterministic and snapshot-backed: the diff is parsed deterministically, symbol
//! spans come from the same tree-sitter extraction as every other tool, and the
//! shared per-snapshot indexed-file set (`shared_indexed_files`) is reused. It is
//! **not** a scope/type resolver — a symbol is "affected" purely when its span
//! overlaps a changed line.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::cancel::CancelToken;
use crate::codemap::{CodeSymbol, block_span};
use crate::graph::shared_indexed_files;
use crate::models::NerveError;
use crate::port::CatalogProvider;
use crate::repomap::IndexedFile;
use crate::snapshot::CatalogSnapshot;

const DETECT_CHANGES_NOTE: &str = "Symbols whose tree-sitter block span overlaps a changed line in \
the supplied unified diff (parsed as text; no VCS invoked). Deterministic name/span matching, not \
a scope/type resolver; chain affected symbols into analyze_impact for the dependency blast radius.";

/// Request for [`detect_changes_cancellable`]: a unified diff as text.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectChangesRequest {
    /// A unified diff (e.g. `git diff` output). Only the new-side line numbers are
    /// used; new/untracked files absent from the snapshot are skipped.
    pub diff: String,
}

/// A symbol whose span overlaps a changed line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AffectedSymbol {
    pub name: String,
    pub kind: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// Per-file affected symbols.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedFileImpact {
    pub display_path: String,
    pub affected: Vec<AffectedSymbol>,
}

/// Response for [`detect_changes_cancellable`].
#[derive(Debug, Clone, Serialize)]
pub struct DetectChangesResponse {
    pub changed_files: usize,
    pub affected_symbols: usize,
    pub files: Vec<ChangedFileImpact>,
    pub note: String,
}

/// Map the changed lines of a unified diff to the symbols they touch.
pub fn detect_changes_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &DetectChangesRequest,
    cancel: &CancelToken,
) -> Result<DetectChangesResponse, NerveError> {
    cancel.check_cancelled()?;
    let parsed = parse_unified_diff(&request.diff);
    let files = shared_indexed_files(provider, snapshot, cancel)?;
    let by_path: BTreeMap<&str, &IndexedFile> = files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();

    let mut out_files = Vec::new();
    let mut affected_total = 0usize;
    for parsed_file in &parsed {
        cancel.check_cancelled()?;
        let Some(path) = parsed_file.new_path.as_deref() else {
            continue;
        };
        if parsed_file.changed.is_empty() {
            continue;
        }
        let Some(file) = by_path.get(path) else {
            continue; // new/untracked or out-of-root file: nothing to map against
        };
        let source = String::from_utf8_lossy(&provider.read_bytes(&file.abs_path)?).into_owned();
        let affected = affected_symbols(&file.path, &source, &file.symbols, &parsed_file.changed);
        if affected.is_empty() {
            continue;
        }
        affected_total += affected.len();
        out_files.push(ChangedFileImpact {
            display_path: file.display_path.clone(),
            affected,
        });
    }
    out_files.sort_by(|left, right| left.display_path.cmp(&right.display_path));

    Ok(DetectChangesResponse {
        changed_files: out_files.len(),
        affected_symbols: affected_total,
        files: out_files,
        note: DETECT_CHANGES_NOTE.to_string(),
    })
}

/// Symbols whose block span overlaps a changed line, sorted by start line then
/// name and de-duplicated.
fn affected_symbols(
    rel_path: &str,
    source: &str,
    symbols: &[CodeSymbol],
    changed: &BTreeSet<usize>,
) -> Vec<AffectedSymbol> {
    let last_changed = changed.iter().next_back().copied().unwrap_or(0);
    let mut affected = Vec::new();
    for symbol in symbols {
        // A symbol that starts after the last changed line cannot contain one
        // (spans extend forward from the start), so skip its block_span parse.
        if symbol.line > last_changed {
            continue;
        }
        let (start, end) =
            block_span(rel_path, source, symbol.line).unwrap_or((symbol.line, symbol.line));
        if changed.range(start..=end).next().is_some() {
            affected.push(AffectedSymbol {
                name: symbol.name.clone(),
                kind: symbol.kind.clone(),
                start_line: start,
                end_line: end,
            });
        }
    }
    affected.sort_by(|left, right| {
        left.start_line
            .cmp(&right.start_line)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.kind.cmp(&right.kind))
    });
    affected.dedup();
    affected
}

/// One file's new-side changed line numbers, parsed from a unified diff.
struct ParsedFile {
    new_path: Option<String>,
    changed: BTreeSet<usize>,
}

/// Cursor tracking remaining old/new lines while consuming one hunk body.
struct HunkCursor {
    new_line: usize,
    new_remaining: usize,
    old_remaining: usize,
}

impl HunkCursor {
    fn done(&self) -> bool {
        self.new_remaining == 0 && self.old_remaining == 0
    }
}

/// Parse a unified diff into per-file new-side changed line sets. Hunk bodies are
/// consumed by their declared line counts, so an added line whose content happens
/// to start with `+++ `/`@@` is never mistaken for a header.
fn parse_unified_diff(diff: &str) -> Vec<ParsedFile> {
    let mut files: Vec<ParsedFile> = Vec::new();
    let mut current: Option<ParsedFile> = None;
    let mut hunk: Option<HunkCursor> = None;

    for line in diff.lines() {
        if hunk.as_ref().is_some_and(|cursor| !cursor.done()) {
            let cursor = hunk.as_mut().expect("active hunk");
            match line.as_bytes().first() {
                Some(b'+') => {
                    if let Some(file) = current.as_mut() {
                        file.changed.insert(cursor.new_line);
                    }
                    cursor.new_line += 1;
                    cursor.new_remaining = cursor.new_remaining.saturating_sub(1);
                    continue;
                }
                Some(b'-') => {
                    // Deletion: record the new-side position where content was
                    // removed so the enclosing symbol is still flagged.
                    if let Some(file) = current.as_mut() {
                        file.changed.insert(cursor.new_line);
                    }
                    cursor.old_remaining = cursor.old_remaining.saturating_sub(1);
                    continue;
                }
                Some(b' ') => {
                    cursor.new_line += 1;
                    cursor.new_remaining = cursor.new_remaining.saturating_sub(1);
                    cursor.old_remaining = cursor.old_remaining.saturating_sub(1);
                    continue;
                }
                Some(b'\\') => continue, // "\ No newline at end of file"
                _ => hunk = None,        // unexpected: end the hunk, retry as header
            }
        }

        if let Some(rest) = line.strip_prefix("+++ ") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            current = Some(ParsedFile {
                new_path: parse_new_path(rest),
                changed: BTreeSet::new(),
            });
            hunk = None;
        } else if let Some(rest) = line.strip_prefix("@@") {
            hunk = parse_hunk_header(rest);
        }
        // "--- " and metadata lines (diff --git, index, etc.) are ignored.
    }
    if let Some(file) = current.take() {
        files.push(file);
    }
    files
}

/// Extract the new-file path from a `+++ ` header, stripping a `b/` prefix and any
/// trailing tab-separated timestamp. `/dev/null` (a deletion) yields `None`.
fn parse_new_path(rest: &str) -> Option<String> {
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    if path == "/dev/null" {
        return None;
    }
    let path = path.strip_prefix("b/").unwrap_or(path);
    (!path.is_empty()).then(|| path.to_string())
}

/// Parse `@@ -old_start,old_count +new_start,new_count @@` (counts default 1).
fn parse_hunk_header(rest: &str) -> Option<HunkCursor> {
    let new_spec = rest.split('+').nth(1)?;
    let new_spec = new_spec.split([' ', '@']).next()?;
    let mut new_parts = new_spec.split(',');
    let new_line: usize = new_parts.next()?.trim().parse().ok()?;
    let new_remaining: usize = new_parts
        .next()
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(1);

    let old_spec = rest.split('-').nth(1)?;
    let old_spec = old_spec.split([' ', '@']).next()?;
    let old_remaining: usize = old_spec
        .split(',')
        .nth(1)
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(1);

    Some(HunkCursor {
        new_line,
        new_remaining,
        old_remaining,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed_lines(diff: &str) -> Vec<(Option<String>, Vec<usize>)> {
        parse_unified_diff(diff)
            .into_iter()
            .map(|file| (file.new_path, file.changed.into_iter().collect()))
            .collect()
    }

    #[test]
    fn parses_added_and_context_lines_to_new_side_numbers() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn keep() {}
-fn old() {}
+fn new_one() {}
+fn new_two() {}
 fn tail() {}
";
        let parsed = changed_lines(diff);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0.as_deref(), Some("src/lib.rs"));
        // line 1 context (keep), line 2 old() removed at new pos 2, lines 2&3 added.
        assert_eq!(parsed[0].1, vec![2, 3]);
    }

    #[test]
    fn added_line_content_starting_like_a_header_is_not_a_header() {
        // An added line whose content is "+++ b/evil" must be data, not a header.
        let diff = "\
+++ b/real.rs
@@ -0,0 +1,2 @@
+fn a() {}
++ not a header
";
        let parsed = changed_lines(diff);
        assert_eq!(parsed.len(), 1, "only one real file header");
        assert_eq!(parsed[0].0.as_deref(), Some("real.rs"));
        assert_eq!(parsed[0].1, vec![1, 2]);
    }

    #[test]
    fn dev_null_new_path_is_skipped() {
        let diff = "\
--- a/gone.rs
+++ /dev/null
@@ -1,1 +0,0 @@
-fn gone() {}
";
        let parsed = changed_lines(diff);
        assert_eq!(parsed[0].0, None);
    }

    #[test]
    fn default_hunk_counts_are_one() {
        let diff = "+++ b/x.rs\n@@ -5 +5 @@\n-old\n+new\n";
        let parsed = changed_lines(diff);
        assert_eq!(parsed[0].1, vec![5]);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn maps_diff_to_the_enclosing_changed_symbol_only() {
        use crate::catalog::{FsCatalogProvider, ScanOptions};
        use crate::security::RootPolicy;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn alpha() {\n    let x = 1;\n}\npub fn beta() {\n    let y = 2;\n}\n",
        )
        .expect("write");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let snapshot = provider.snapshot_arc().expect("snapshot");

        // Change line 2 — inside `alpha`'s body; `beta` (lines 4-6) is untouched.
        let diff = "\
--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
 pub fn alpha() {
-    let x = 1;
+    let x = 42;
 }
";
        let request = DetectChangesRequest {
            diff: diff.to_string(),
        };
        let response =
            detect_changes_cancellable(&provider, &snapshot, &request, &CancelToken::never())
                .expect("detect_changes");

        assert_eq!(response.files.len(), 1, "exactly the one changed file");
        let names: Vec<&str> = response.files[0]
            .affected
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert!(
            names.contains(&"alpha"),
            "alpha (its body line changed) must be affected; got {names:?}"
        );
        assert!(
            !names.contains(&"beta"),
            "beta (unchanged) must not be affected; got {names:?}"
        );
    }
}
