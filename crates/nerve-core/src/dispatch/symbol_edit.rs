use super::*;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub(super) struct SymbolBodyEditArgs {
    pub(super) symbol: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(default)]
    pub(super) kind: Option<String>,
    pub(super) body: String,
}

enum InsertPosition {
    Before,
    After,
}

pub(super) fn handle_replace_symbol_body<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: SymbolBodyEditArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let request = read_request(&args);
    let found = crate::navigate::read_symbol_cancellable(provider, &snapshot, &request, cancel)?;
    cancel.check_cancelled()?;
    let Some(body) = found.body else {
        return replace_symbol_body_noop(found);
    };
    let edit_path = edit_path_for_body(&snapshot, &body);
    let response_path = body.display_path.clone();
    let old = read_current(provider, &edit_path)?;
    if span_content(&old, body.start_line, body.end_line) != body.content {
        return symbol_edit_stale("replace_symbol_body", &args.symbol, &body);
    }
    let updated = replace_line_span(&old, body.start_line, body.end_line, &args.body);
    tool_response_text(&apply_content_update_at_path_with_old(
        provider,
        "replace_symbol_body",
        edit_path,
        response_path,
        updated,
        old,
        DiffOptions::default(),
    )?)
}

pub(super) fn handle_insert_before_symbol<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    handle_insert_symbol(
        provider,
        arguments,
        cancel,
        InsertPosition::Before,
        "insert_before_symbol",
    )
}

pub(super) fn handle_insert_after_symbol<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    handle_insert_symbol(
        provider,
        arguments,
        cancel,
        InsertPosition::After,
        "insert_after_symbol",
    )
}

fn handle_insert_symbol<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
    position: InsertPosition,
    action: &'static str,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: SymbolBodyEditArgs = serde_json::from_value(arguments)?;
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let request = read_request(&args);
    let found = crate::navigate::read_symbol_cancellable(provider, &snapshot, &request, cancel)?;
    cancel.check_cancelled()?;
    let Some(body) = found.body else {
        return symbol_edit_noop(action, found);
    };
    let edit_path = edit_path_for_body(&snapshot, &body);
    let response_path = body.display_path.clone();
    let old = read_current(provider, &edit_path)?;
    if span_content(&old, body.start_line, body.end_line) != body.content {
        return symbol_edit_stale(action, &args.symbol, &body);
    }
    let updated = insert_at_symbol_span(&old, body.start_line, body.end_line, &args.body, position);
    tool_response_text(&apply_content_update_at_path_with_old(
        provider,
        action,
        edit_path,
        response_path,
        updated,
        old,
        DiffOptions::default(),
    )?)
}

fn read_request(args: &SymbolBodyEditArgs) -> crate::navigate::ReadSymbolRequest {
    crate::navigate::ReadSymbolRequest {
        symbol: args.symbol.clone(),
        path: args.path.clone(),
        language: args.language.clone(),
        kind: args.kind.clone(),
        include_body: true,
        max_matches: 20,
    }
}

fn edit_path_for_body(
    snapshot: &crate::CatalogSnapshot,
    body: &crate::navigate::ReadSymbolBody,
) -> String {
    snapshot
        .entries
        .iter()
        .find(|entry| {
            entry.rel_path == body.path
                && snapshot_display_path(snapshot, &entry.root_id, &entry.rel_path)
                    == body.display_path
        })
        .map(|entry| entry.abs_path.to_string_lossy().into_owned())
        .unwrap_or_else(|| body.path.clone())
}

fn snapshot_display_path(
    snapshot: &crate::CatalogSnapshot,
    root_id: &str,
    rel_path: &str,
) -> String {
    if snapshot.roots.len() <= 1 {
        return rel_path.to_string();
    }
    format!("{root_id}/{rel_path}")
}

fn replace_symbol_body_noop(
    response: crate::navigate::ReadSymbolResponse,
) -> Result<Value, DispatchError> {
    symbol_edit_noop("replace_symbol_body", response)
}

fn symbol_edit_noop(
    action: &str,
    response: crate::navigate::ReadSymbolResponse,
) -> Result<Value, DispatchError> {
    let text = if response.total == 0 {
        format!("{action}: no matches for {}\n", response.symbol)
    } else {
        format!(
            "{action}: {} matches for {} (ambiguous; refine with path, language, or kind)\n",
            response.total, response.symbol
        )
    };
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": {
            "mutated": false,
            "symbol": response.symbol,
            "matches": response.matches,
            "total": response.total,
            "truncated": response.truncated,
            "note": response.note,
        },
    }))
}

fn symbol_edit_stale(
    action: &str,
    symbol: &str,
    body: &crate::navigate::ReadSymbolBody,
) -> Result<Value, DispatchError> {
    let text = format!(
        "{action}: {} changed since symbol lookup; retry read_symbol and reapply\n",
        body.display_path
    );
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": {
            "mutated": false,
            "symbol": symbol,
            "path": body.path,
            "display_path": body.display_path,
            "start_line": body.start_line,
            "end_line": body.end_line,
            "reason": "stale_symbol_span",
            "reread_hint": format!("Call read_symbol again, then retry {action} with the updated symbol body.")
        },
    }))
}

fn read_current<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: &str,
) -> Result<String, DispatchError> {
    Ok(String::from_utf8_lossy(&provider.read_bytes(Path::new(path))?).into_owned())
}

fn span_content(source: &str, start_line: usize, end_line: usize) -> String {
    let lines = split_lines_preserve(source);
    if lines.is_empty() {
        return String::new();
    }
    let start = start_line.saturating_sub(1).min(lines.len());
    let end = end_line.max(start_line).min(lines.len());
    lines[start..end].concat()
}

fn insert_at_symbol_span(
    source: &str,
    start_line: usize,
    end_line: usize,
    insertion: &str,
    position: InsertPosition,
) -> String {
    let lines = split_lines_preserve(source);
    let newline = newline_for(source);
    let mut block = normalize_inserted_block(insertion, newline);
    let index = match position {
        InsertPosition::Before => start_line.saturating_sub(1).min(lines.len()),
        InsertPosition::After => end_line.min(lines.len()),
    };
    if matches!(position, InsertPosition::After)
        && !block.is_empty()
        && !block.starts_with(newline)
        && index > 0
        && !lines[index - 1].ends_with('\n')
    {
        block.insert_str(0, newline);
    }
    let mut next = String::new();
    next.push_str(&lines[..index].concat());
    next.push_str(&block);
    next.push_str(&lines[index..].concat());
    next
}

fn replace_line_span(
    source: &str,
    start_line: usize,
    end_line: usize,
    replacement: &str,
) -> String {
    let lines = split_lines_preserve(source);
    if lines.is_empty() {
        return normalize_replacement(replacement, newline_for(source));
    }
    let start = start_line.saturating_sub(1).min(lines.len());
    let end = end_line.max(start_line).min(lines.len());
    let newline = newline_for(source);
    let mut next = String::new();
    next.push_str(&lines[..start].concat());
    let removed_had_newline = lines[end.saturating_sub(1)..end]
        .first()
        .is_some_and(|line| line.ends_with('\n'));
    let mut body = normalize_replacement(replacement, newline);
    if removed_had_newline && !body.ends_with('\n') {
        body.push_str(newline);
    }
    next.push_str(&body);
    next.push_str(&lines[end..].concat());
    next
}

fn split_lines_preserve(source: &str) -> Vec<&str> {
    source.split_inclusive('\n').collect()
}

fn normalize_replacement(replacement: &str, newline: &str) -> String {
    let trimmed = replacement.trim_matches(['\r', '\n']);
    normalize_newlines(trimmed, newline)
}

fn normalize_inserted_block(insertion: &str, newline: &str) -> String {
    let mut block = normalize_newlines(insertion, newline);
    if !block.is_empty() && !block.ends_with(newline) {
        block.push_str(newline);
    }
    block
}

fn normalize_newlines(text: &str, newline: &str) -> String {
    if newline == "\n" {
        return text.replace("\r\n", "\n").replace('\r', "\n");
    }
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', newline)
}

fn newline_for(source: &str) -> &str {
    if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}
