//! Read-file operation with optional line bounds.

use crate::{models::*, port::CatalogProvider};

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

    let content = if total_lines == 0 {
        String::new()
    } else {
        line_segments[start - 1..end].concat()
    };

    Ok(ReadFileResponse {
        path: request.path.clone(),
        display_path: provider.display_path(&request.path),
        first_line: start,
        last_line: end,
        total_lines,
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsCatalogProvider, RootPolicy, catalog::ScanOptions};
    use std::fs;

    #[test]
    fn slices_lines_and_preserves_newlines() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.txt"),
                start_line: Some(2),
                end_line: Some(3),
                limit: None,
            },
        )
        .expect("read");
        assert_eq!(response.content, "two\nthree\n");
    }

    #[test]
    fn limit_wins_over_open_ended_slice() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").expect("write");
        let provider = FsCatalogProvider::new(
            RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            ScanOptions::default(),
        );
        let response = read_file(
            &provider,
            &ReadFileRequest {
                path: dir.path().join("a.txt"),
                start_line: Some(2),
                end_line: None,
                limit: Some(1),
            },
        )
        .expect("read");
        assert_eq!(response.first_line, 2);
        assert_eq!(response.last_line, 2);
        assert_eq!(response.content, "two\n");
    }
}
