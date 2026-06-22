use super::{
    RenderedFile, SelectedFile, WorkspaceContextFileTokens, WorkspaceContextSegmentTokens,
    content_hash, label, mode_name,
};
use crate::{
    codemap::FileCodeStructure, models::NerveError, port::CatalogProvider, selection::LineRange,
    selection::SelectionMode, token::count_tokens,
};

pub(crate) fn render_selected_file<P: CatalogProvider>(
    provider: &P,
    selected: &SelectedFile<'_>,
) -> Result<RenderedFile, NerveError> {
    match &selected.mode {
        SelectionMode::Full => render_full_file(provider, selected),
        SelectionMode::Slices(ranges) => render_slices_file(provider, selected, ranges),
        SelectionMode::CodemapOnly => render_codemap_file(provider, selected),
    }
}

fn render_full_file<P: CatalogProvider>(
    provider: &P,
    selected: &SelectedFile<'_>,
) -> Result<RenderedFile, NerveError> {
    let bytes = provider.read_bytes(&selected.entry.abs_path)?;
    let content = String::from_utf8_lossy(&bytes);
    let segment_tokens = count_tokens(&content);
    let text = format!(
        "<file path=\"{}\" mode=\"full\">\n```text\n{}```\n</file>",
        selected.display_path, content
    );
    let file_token_count = count_tokens(&text);
    let tokens = file_tokens(
        selected,
        file_token_count,
        &text,
        vec![WorkspaceContextSegmentTokens {
            label: "full".to_string(),
            start_line: Some(1),
            end_line: Some(total_lines(&content)),
            token_count: segment_tokens,
        }],
    );
    Ok(RenderedFile { text, tokens })
}

fn render_slices_file<P: CatalogProvider>(
    provider: &P,
    selected: &SelectedFile<'_>,
    ranges: &[LineRange],
) -> Result<RenderedFile, NerveError> {
    let bytes = provider.read_bytes(&selected.entry.abs_path)?;
    let content = String::from_utf8_lossy(&bytes);
    let mut text = format!(
        "<file path=\"{}\" mode=\"slices\">\n",
        selected.display_path
    );
    let mut segments = Vec::new();
    for range in ranges {
        let slice = slice_text(&content, range);
        let label = label::range_label(range);
        let token_count = count_tokens(&slice);
        text.push_str(&format!(
            "<slice lines=\"{}-{}\" description=\"{}\">\n```text\n{}```\n</slice>\n",
            range.start_line,
            range.end_line,
            label::escape_attr(&label),
            slice
        ));
        segments.push(WorkspaceContextSegmentTokens {
            label,
            start_line: Some(range.start_line),
            end_line: Some(range.end_line),
            token_count,
        });
    }
    text.push_str("</file>");
    let token_count = count_tokens(&text);
    let tokens = file_tokens(selected, token_count, &text, segments);
    Ok(RenderedFile { text, tokens })
}

fn render_codemap_file<P: CatalogProvider>(
    provider: &P,
    selected: &SelectedFile<'_>,
) -> Result<RenderedFile, NerveError> {
    let (codemap_text, segment_tokens) =
        match provider.code_symbols_for_path(&selected.entry.abs_path, &selected.entry.rel_path)? {
            Ok(Some(parsed)) => {
                let structure = FileCodeStructure {
                    path: selected.entry.rel_path.clone(),
                    language: parsed.language.clone(),
                    symbols: parsed.symbols.clone(),
                    token_count: 0,
                };
                let text = render_codemap_signature(&structure);
                let tokens = count_tokens(&text);
                (text, tokens)
            }
            Ok(None) => ("unsupported file for codemap\n".to_string(), 0),
            Err(message) => (format!("codemap error: {message}\n"), 0),
        };
    let text = format!(
        "<file path=\"{}\" mode=\"codemap_only\">\n```text\n{}```\n</file>",
        selected.display_path, codemap_text
    );
    let file_token_count = count_tokens(&text);
    let tokens = file_tokens(
        selected,
        file_token_count,
        &text,
        vec![WorkspaceContextSegmentTokens {
            label: "codemap".to_string(),
            start_line: None,
            end_line: None,
            token_count: segment_tokens,
        }],
    );
    Ok(RenderedFile { text, tokens })
}

fn render_codemap_signature(structure: &FileCodeStructure) -> String {
    let mut lines = vec![format!("language: {}", structure.language)];
    for symbol in &structure.symbols {
        lines.push(format!(
            "- {} {} @ line {}",
            symbol.kind, symbol.name, symbol.line
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn slice_text(text: &str, range: &LineRange) -> String {
    let line_segments: Vec<&str> = text.split_inclusive('\n').collect();
    if line_segments.is_empty() {
        return String::new();
    }
    let start = range.start_line.max(1).min(line_segments.len());
    let end = range.end_line.max(start).min(line_segments.len());
    line_segments[start - 1..end].concat()
}

fn total_lines(text: &str) -> usize {
    text.split_inclusive('\n').count().max(1)
}

fn file_tokens(
    selected: &SelectedFile<'_>,
    token_count: usize,
    rendered_text: &str,
    segments: Vec<WorkspaceContextSegmentTokens>,
) -> WorkspaceContextFileTokens {
    WorkspaceContextFileTokens {
        root_id: selected.key.root_id.clone(),
        path: selected.key.path.clone(),
        display_path: selected.display_path.clone(),
        mode: mode_name(&selected.mode).to_string(),
        content_hash: content_hash(rendered_text),
        token_count,
        segments,
    }
}
