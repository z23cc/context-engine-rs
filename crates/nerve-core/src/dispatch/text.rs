use super::DispatchError;
use serde_json::{Value, json};

pub(super) fn tool_response(structured: Value) -> Result<Value, DispatchError> {
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&structured)? }],
        "structuredContent": structured,
    }))
}

/// Wrap a tool response so the model-facing `content[].text` is a compact,
/// readable rendering while the full data stays in `structuredContent`. This
/// avoids dumping verbose JSON (escaped newlines, repeated keys) at the model.
pub(super) fn tool_response_text<T>(response: &T) -> Result<Value, DispatchError>
where
    T: serde::Serialize + ToolText,
{
    let text = response.tool_text();
    let structured = serde_json::to_value(response)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    }))
}

/// Compact text rendering used for a tool's `content[].text`.
pub(super) trait ToolText {
    fn tool_text(&self) -> String;
}

impl ToolText for crate::ReadFileResponse {
    fn tool_text(&self) -> String {
        self.content.clone()
    }
}

impl ToolText for crate::FileTreeResponse {
    fn tool_text(&self) -> String {
        let mut sections = Vec::new();
        if !self.tree.is_empty() {
            sections.push(self.tree.clone());
        }
        if self.uses_legend {
            sections.push("legend: * selected, + codemap-capable".to_string());
        }
        if let Some(note) = self.note.as_deref().filter(|n| !n.is_empty()) {
            sections.push(format!("(note: {note})"));
        }
        sections.join("\n\n")
    }
}

impl ToolText for crate::WorkspaceContextResponse {
    fn tool_text(&self) -> String {
        self.context.clone()
    }
}

impl ToolText for crate::BuildContextResponse {
    fn tool_text(&self) -> String {
        if self.manifest.sensitive_findings.is_empty() {
            return self.context.clone();
        }
        format!(
            "warning: {} sensitive-content findings in structuredContent\n\n{}",
            self.manifest.sensitive_findings.len(),
            self.context
        )
    }
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl ToolText for crate::SemanticSearchResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        if self.results.is_empty() {
            out.push_str("(no semantic matches)\n");
        } else {
            out.push_str("semantic matches:\n");
            for result in &self.results {
                let symbol = result
                    .symbol
                    .as_deref()
                    .map(|symbol| format!(" {symbol}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {}:{}-{}{} (score {:.4})\n",
                    result.display_path, result.line_start, result.line_end, symbol, result.score
                ));
            }
        }
        out.push_str(&format!(
            "totals: {} chunks, {} dense, {} bm25, {} reranked\n",
            self.totals.chunks,
            self.totals.dense_candidates,
            self.totals.bm25_candidates,
            self.totals.reranked
        ));
        if !self.diagnostics.is_empty() {
            out.push_str(&format!(
                "({} diagnostics in structuredContent)\n",
                self.diagnostics.len()
            ));
        }
        out
    }
}

impl ToolText for crate::SearchResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        // Summary header first so a model reading top-down learns the result
        // shape — and whether it was truncated — before scanning the matches.
        let totals = &self.totals;
        out.push_str(&format!(
            "search: {} content, {} path · {} files scanned",
            totals.content_matches, totals.path_matches, totals.scanned_files
        ));
        if totals.totals_are_lower_bound || totals.omitted > 0 {
            out.push_str(" · TRUNCATED (results capped — narrow the query)");
        }
        out.push('\n');

        // Cheap relevance overview: the files carrying the most content hits.
        let top = top_content_files(&self.content_matches);
        if !top.is_empty() {
            let rendered: Vec<String> = top
                .iter()
                .map(|(path, n)| format!("{path} ({n})"))
                .collect();
            out.push_str(&format!("top files: {}\n", rendered.join(", ")));
        }

        if !self.path_matches.is_empty() {
            out.push_str("path matches:\n");
            for m in &self.path_matches {
                out.push_str("  ");
                out.push_str(&m.display_path);
                out.push('\n');
            }
        }
        if !self.content_matches.is_empty() {
            out.push_str("content matches:\n");
            for m in &self.content_matches {
                out.push_str(&format!(
                    "  {}:{}:{}: {}\n",
                    m.display_path,
                    m.line,
                    m.column,
                    m.text.trim_end()
                ));
            }
        }
        if !self.match_files.is_empty() {
            out.push_str("matching files:\n");
            for f in &self.match_files {
                out.push_str(&format!("  {} ({})\n", f.display_path, f.count));
            }
        }
        if self.path_matches.is_empty()
            && self.content_matches.is_empty()
            && self.match_files.is_empty()
        {
            out.push_str("(no matches)\n");
        }
        out
    }
}

/// The content-hit files carrying the most matches, sorted by count (desc) then
/// path (asc) for deterministic output. Capped at the top 5; returns empty when
/// fewer than two files matched, since the per-line list already makes a single
/// file's relevance obvious.
fn top_content_files(matches: &[crate::ContentSearchMatch]) -> Vec<(String, usize)> {
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for m in matches {
        *counts.entry(m.display_path.as_str()).or_insert(0) += 1;
    }
    if counts.len() < 2 {
        return Vec::new();
    }
    let mut ranked: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(p, n)| (p.to_string(), n))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(5);
    ranked
}

/// Character budget for the rendered repo-map text (~3k tokens). Like aider's
/// `map_tokens`, the map degrades to fit: top-ranked files render first and the
/// tail is dropped with a note, so a large `max_files` on a big repo never blows
/// up the model-facing text. Full ranking stays in `structuredContent`.
pub(super) const REPO_MAP_TEXT_BUDGET_CHARS: usize = 12_000;
const REPO_MAP_MAX_NAMES: usize = 8;

pub(super) fn render_repo_map_text(
    response: &crate::repomap::RepoMapResponse,
    budget: usize,
) -> String {
    let mut out = String::new();
    let mut rendered = 0usize;
    for file in &response.files {
        let mut line = String::new();
        line.push_str(&file.score);
        line.push('\t');
        line.push_str(&file.display_path);
        let names: Vec<&str> = file
            .symbols
            .iter()
            .take(REPO_MAP_MAX_NAMES)
            .map(|s| s.name.as_str())
            .collect();
        if !names.is_empty() {
            line.push('\t');
            line.push_str(&names.join(", "));
            if file.symbols.len() > names.len() {
                line.push_str(", …");
            }
        }
        line.push('\n');
        // Always emit at least the top-ranked file; stop once the budget is hit.
        if rendered > 0 && out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
        rendered += 1;
    }
    if response.files.is_empty() {
        out.push_str("(no ranked files)\n");
    }
    let omitted = response.files.len() - rendered;
    if omitted > 0 {
        out.push_str(&format!(
            "(+{omitted} more ranked files omitted to fit the map budget; full ranking in structuredContent)\n"
        ));
    }
    if !response.diagnostics.is_empty() {
        out.push_str(&format!(
            "({} files skipped; parse diagnostics in structuredContent)\n",
            response.diagnostics.len()
        ));
    }
    out
}

impl ToolText for crate::repomap::RepoMapResponse {
    fn tool_text(&self) -> String {
        render_repo_map_text(self, REPO_MAP_TEXT_BUDGET_CHARS)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ToolText for crate::workspace::ManageWorkspacesResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for ws in &self.workspaces {
            let roots: Vec<String> = ws.roots.iter().map(|r| r.display().to_string()).collect();
            out.push_str(&format!("{}\t{}\n", ws.name, roots.join(", ")));
        }
        if self.workspaces.is_empty() {
            out.push_str("(no workspaces)\n");
        }
        if let Some(changed) = &self.changed {
            out.push_str(&format!("changed: {changed}\n"));
        }
        out
    }
}

impl ToolText for crate::navigate::SymbolSearchResponse {
    fn tool_text(&self) -> String {
        if self.matches.is_empty() {
            if self.total > 0 {
                return format!(
                    "symbol_search: {} matches for {:?} (showing 0)\n",
                    self.total, self.query
                );
            }
            return format!("symbol_search: no matches for {:?}\n", self.query);
        }
        let mut out = format!(
            "symbol_search: {} matches for {:?}\n",
            self.total, self.query
        );
        for item in &self.matches {
            match &item.signature {
                Some(signature) => out.push_str(&format!(
                    "  {}:{} {} {} (score {})\n",
                    item.display_path, item.line, item.kind, signature, item.score
                )),
                None => out.push_str(&format!(
                    "  {}:{} {} {} (score {})\n",
                    item.display_path, item.line, item.kind, item.name, item.score
                )),
            }
        }
        if self.truncated {
            out.push_str(&format!(
                "(showing {} of {})\n",
                self.matches.len(),
                self.total
            ));
        }
        out
    }
}

#[path = "text_nav.rs"]
mod text_nav;

impl ToolText for crate::codemap::CodeStructureResponse {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            out.push_str(&crate::codemap::render_file_codemap(file));
        }
        if self.files.is_empty() {
            out.push_str("(no symbols)\n");
        }
        if self.omitted > 0 {
            out.push_str(&format!(
                "({} files omitted: unsupported or no symbols)\n",
                self.omitted
            ));
        }
        if !self.diagnostics.is_empty() {
            out.push_str(&format!(
                "({} parse diagnostics in structuredContent)\n",
                self.diagnostics.len()
            ));
        }
        out
    }
}
