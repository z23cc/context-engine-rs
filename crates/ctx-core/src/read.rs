//! Read-file operation with optional line bounds.

use crate::{
    codemap::{self, ContainingBlockError},
    models::*,
    port::CatalogProvider,
};

/// Read a UTF-8-lossy file slice after provider containment checks.
pub fn read_file<P: CatalogProvider>(
    provider: &P,
    request: &ReadFileRequest,
) -> Result<ReadFileResponse, CtxError> {
    let bytes = provider.read_bytes(&request.path)?;
    let text = String::from_utf8_lossy(&bytes);
    let line_segments: Vec<&str> = text.split_inclusive('\n').collect();
    let total_lines = line_segments.len();
    let start = request
        .start_line
        .unwrap_or(1)
        .max(1)
        .min(total_lines.max(1));
    let end = if total_lines == 0 {
        start
    } else if let Some(limit) = request.limit {
        start.saturating_add(limit.max(1)).saturating_sub(1)
    } else {
        request.end_line.unwrap_or(total_lines)
    }
    .max(start)
    .min(total_lines.max(start));

    let display_path = provider.display_path(&request.path);
    let snap = snap_metadata(request.snap, &display_path, &text, total_lines, start, end);
    let (first_line, last_line) = snap.as_ref().map_or((start, end), |metadata| {
        (metadata.returned_first_line, metadata.returned_last_line)
    });

    let content = if total_lines == 0 {
        String::new()
    } else {
        line_segments[first_line - 1..last_line].concat()
    };

    Ok(ReadFileResponse {
        path: request.path.clone(),
        display_path,
        first_line,
        last_line,
        total_lines,
        content,
        snap,
    })
}

fn snap_metadata(
    mode: Option<ReadFileSnapMode>,
    display_path: &str,
    text: &str,
    total_lines: usize,
    start: usize,
    end: usize,
) -> Option<ReadFileSnapMetadata> {
    match mode? {
        ReadFileSnapMode::None => None,
        ReadFileSnapMode::Block => Some(block_snap_metadata(
            display_path,
            text,
            total_lines,
            start,
            end,
        )),
    }
}

fn block_snap_metadata(
    display_path: &str,
    text: &str,
    total_lines: usize,
    start: usize,
    end: usize,
) -> ReadFileSnapMetadata {
    if let Some(span) = codemap::block_span(display_path, text, start)
        && span.1 > span.0
    {
        return applied_snap(start, end, clamp_span(span, total_lines));
    }
    match codemap::containing_block_span(display_path, text, start, end) {
        Ok(Some(span)) => applied_snap(start, end, clamp_span(span, total_lines)),
        Ok(None) => unapplied_snap(start, end, "no_containing_block"),
        Err(error) => unapplied_snap(start, end, containing_block_reason(error)),
    }
}

fn applied_snap(
    requested_first: usize,
    requested_last: usize,
    returned: (usize, usize),
) -> ReadFileSnapMetadata {
    ReadFileSnapMetadata {
        mode: ReadFileSnapMode::Block,
        applied: true,
        reason: None,
        requested_first_line: requested_first,
        requested_last_line: requested_last,
        returned_first_line: returned.0,
        returned_last_line: returned.1,
        boundary_lines: boundary_lines(requested_first, requested_last, returned),
    }
}

fn unapplied_snap(
    requested_first: usize,
    requested_last: usize,
    reason: &str,
) -> ReadFileSnapMetadata {
    ReadFileSnapMetadata {
        mode: ReadFileSnapMode::Block,
        applied: false,
        reason: Some(reason.to_string()),
        requested_first_line: requested_first,
        requested_last_line: requested_last,
        returned_first_line: requested_first,
        returned_last_line: requested_last,
        boundary_lines: Vec::new(),
    }
}

fn clamp_span((first, last): (usize, usize), total_lines: usize) -> (usize, usize) {
    let first = first.max(1).min(total_lines.max(1));
    let last = last.max(first).min(total_lines.max(first));
    (first, last)
}

fn boundary_lines(
    requested_first: usize,
    requested_last: usize,
    returned: (usize, usize),
) -> Vec<usize> {
    let mut lines = Vec::new();
    if returned.0 < requested_first {
        lines.push(returned.0);
    }
    if returned.1 > requested_last && returned.1 != returned.0 {
        lines.push(returned.1);
    }
    lines
}

fn containing_block_reason(error: ContainingBlockError) -> &'static str {
    match error {
        ContainingBlockError::UnsupportedLanguage => "unsupported_language",
        ContainingBlockError::ParseError => "syntax_error",
        ContainingBlockError::BlankLine => "blank_line",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, catalog::ScanOptions};
    use std::fs;

    fn provider_for(root: &std::path::Path) -> FsCatalogProvider {
        FsCatalogProvider::new(
            RootPolicy::new(vec![root.to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        )
    }

    #[test]
    fn slices_lines_and_preserves_newlines() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
        let provider = provider_for(dir.path());
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.txt"),
                start_line: Some(2),
                end_line: Some(3),
                limit: None,
                snap: None,
            },
        )
        .expect("read");
        assert_eq!(response.content, "two\nthree\n");
    }

    #[test]
    fn limit_wins_over_open_ended_slice() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
        let provider = provider_for(dir.path());
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.txt"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
                snap: None,
            },
        )
        .expect("read");
        assert_eq!(response.first_line, 2);
        assert_eq!(response.last_line, 2);
        assert_eq!(response.content, "two\n");
    }

    #[test]
    fn snap_none_matches_raw_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "fn a() {\n    let x = 1;\n}\n").expect("write");
        let provider = provider_for(dir.path());
        let raw = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.rs"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
                snap: None,
            },
        )
        .expect("raw read");
        let none = read_file(
            &provider,
            &ReadFileRequest {
                snap: Some(ReadFileSnapMode::None),
                ..ReadFileRequest {
                    path: dir.path().join("a.rs"),
                    start_line: Some(2),
                    end_line: None,
                    limit: Some(1),
                    snap: None,
                }
            },
        )
        .expect("snap none read");
        assert_eq!(none.content, raw.content);
        assert_eq!(none.first_line, raw.first_line);
        assert_eq!(none.last_line, raw.last_line);
        assert_eq!(none.snap, None);
    }

    #[test]
    fn snap_block_expands_from_opener_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "fn a() {\n    let x = 1;\n}\n").expect("write");
        let provider = provider_for(dir.path());
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.rs"),
                start_line: Some(1),
                end_line: None,
                limit: Some(1),
                snap: Some(ReadFileSnapMode::Block),
            },
        )
        .expect("read");
        assert_eq!(response.first_line, 1);
        assert_eq!(response.last_line, 3);
        assert_eq!(response.content, "fn a() {\n    let x = 1;\n}\n");
        let snap = response.snap.expect("snap metadata");
        assert!(snap.applied);
        assert_eq!(snap.boundary_lines, vec![3]);
    }

    #[test]
    fn snap_block_expands_from_interior_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "fn a() {\n    let x = 1;\n}\n").expect("write");
        let provider = provider_for(dir.path());
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.rs"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
                snap: Some(ReadFileSnapMode::Block),
            },
        )
        .expect("read");
        assert_eq!(response.first_line, 1);
        assert_eq!(response.last_line, 3);
        let snap = response.snap.expect("snap metadata");
        assert_eq!(snap.boundary_lines, vec![1, 3]);
    }

    #[test]
    fn snap_block_unsupported_file_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
        let provider = provider_for(dir.path());
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.txt"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
                snap: Some(ReadFileSnapMode::Block),
            },
        )
        .expect("read");
        assert_eq!(response.content, "two\n");
        assert_eq!(
            response.snap.expect("snap metadata").reason.as_deref(),
            Some("unsupported_language")
        );
    }

    #[test]
    fn snap_block_syntax_error_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "fn broken( {\n    let x = 1;\n}\n").expect("write");
        let provider = provider_for(dir.path());
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.rs"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
                snap: Some(ReadFileSnapMode::Block),
            },
        )
        .expect("read");
        assert_eq!(response.content, "    let x = 1;\n");
        assert_eq!(
            response.snap.expect("snap metadata").reason.as_deref(),
            Some("syntax_error")
        );
    }
}
