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
) -> Result<ReadFileResponse, NerveError> {
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
    match codemap::embedded_block_span(display_path, text, start, end) {
        Ok(Some(span)) => return applied_snap(start, end, clamp_span(span, total_lines)),
        Ok(None) => {}
        Err(error) => return unapplied_snap(start, end, containing_block_reason(error)),
    }
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
