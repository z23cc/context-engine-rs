use super::{DispatchError, ToolText, edit};
use crate::{CatalogProvider, edit::FileChange};
use std::path::Path;

/// Adapts a [`CatalogProvider`] into an [`edit::FileReader`]; reads are
/// containment-checked by the provider's root policy.
pub(super) struct ProviderReader<'a, P: CatalogProvider + ?Sized> {
    pub(super) provider: &'a P,
}

impl<P: CatalogProvider + ?Sized> edit::FileReader for ProviderReader<'_, P> {
    fn read_text(&self, path: &str) -> Option<String> {
        self.provider
            .read_bytes(Path::new(path))
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[derive(serde::Serialize)]
pub(super) struct EditedFile {
    pub(super) action: &'static str,
    pub(super) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) moved_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) view: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) diff: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) diagnostics: Vec<crate::codemap::SyntaxIssue>,
}

impl EditedFile {
    pub(super) fn with_content(
        action: &'static str,
        path: String,
        moved_to: Option<String>,
        content: &str,
        old: &str,
        diff_options: DiffOptions,
    ) -> Self {
        let display = moved_to.clone().unwrap_or_else(|| path.clone());
        let diff = (old != content).then(|| {
            if diff_options == DiffOptions::default() {
                unified_diff(&display, old, content)
            } else {
                unified_diff_with_options(&display, old, content, diff_options)
            }
        });
        Self {
            action,
            tag: Some(edit::snapshot_tag(content)),
            view: Some(edit::hashline_view(&display, content)),
            diff,
            diagnostics: crate::codemap::syntax_diagnostics(&display, content),
            path,
            moved_to,
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct EditResult {
    pub(super) files: Vec<EditedFile>,
}

impl ToolText for EditResult {
    fn tool_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            match &file.moved_to {
                Some(to) => out.push_str(&format!("{} {} -> {}\n", file.action, file.path, to)),
                None => out.push_str(&format!("{} {}\n", file.action, file.path)),
            }
        }
        for file in &self.files {
            for issue in &file.diagnostics {
                out.push_str(&format!(
                    "  \u{26a0} {} line {}: {}\n",
                    file.path, issue.line, issue.message
                ));
            }
        }
        for file in &self.files {
            if let Some(diff) = &file.diff {
                out.push('\n');
                out.push_str(diff);
            }
        }
        out
    }
}

pub(super) fn apply_changes<P: CatalogProvider + ?Sized>(
    provider: &P,
    changes: Vec<FileChange>,
    diff_options: DiffOptions,
) -> Result<EditResult, DispatchError> {
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        let edited = match change {
            FileChange::Create { path, content } => {
                provider.write_text(Path::new(&path), &content)?;
                EditedFile::with_content("create", path, None, &content, "", diff_options)
            }
            FileChange::Update { path, content } => {
                let old = read_old(provider, &path);
                provider.write_text(Path::new(&path), &content)?;
                EditedFile::with_content("update", path, None, &content, &old, diff_options)
            }
            FileChange::Delete { path } => {
                provider.delete_file(Path::new(&path))?;
                EditedFile {
                    action: "delete",
                    path,
                    moved_to: None,
                    tag: None,
                    view: None,
                    diff: None,
                    diagnostics: Vec::new(),
                }
            }
            FileChange::Rename { from, to, content } => {
                let old = read_old(provider, &from);
                provider.rename_file(Path::new(&from), Path::new(&to))?;
                provider.write_text(Path::new(&to), &content)?;
                EditedFile::with_content("rename", from, Some(to), &content, &old, diff_options)
            }
        };
        files.push(edited);
    }
    Ok(EditResult { files })
}

/// Current text of `path`, or empty if it does not exist / is unreadable.
pub(super) fn read_old<P: CatalogProvider + ?Sized>(provider: &P, path: &str) -> String {
    provider
        .read_bytes(Path::new(path))
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DiffOptions {
    pub(super) context_lines: usize,
    pub(super) ignore_whitespace: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context_lines: 3,
            ignore_whitespace: false,
        }
    }
}

/// A compact unified diff (old -> new) for the edit response.
pub(super) fn unified_diff(path: &str, old: &str, new: &str) -> String {
    unified_diff_with_options(path, old, new, DiffOptions::default())
}

pub(super) fn unified_diff_with_options(
    path: &str,
    old: &str,
    new: &str,
    options: DiffOptions,
) -> String {
    let adjusted_new = options
        .ignore_whitespace
        .then(|| whitespace_filtered_new(old, new));
    let new = adjusted_new.as_deref().unwrap_or(new);
    let text_diff = similar::TextDiff::from_lines(old, new);
    let mut builder = text_diff.unified_diff();
    builder
        .context_radius(options.context_lines)
        .header(&format!("a/{path}"), &format!("b/{path}"));
    cap_diff(builder.to_string())
}

fn whitespace_filtered_new(old: &str, new: &str) -> String {
    let old_lines = line_units(old);
    let mut new_lines: Vec<String> = line_units(new).into_iter().map(str::to_string).collect();
    let text_diff = similar::TextDiff::from_lines(old, new);
    for op in text_diff.ops() {
        let old_range = op.old_range();
        let new_range = op.new_range();
        let old_len = old_range.len();
        let new_len = new_range.len();
        if old_len > 0 && new_len > 0 {
            for offset in 0..old_len.min(new_len) {
                let old_line = old_lines[old_range.start + offset];
                let new_slot = &mut new_lines[new_range.start + offset];
                if normalize_whitespace(old_line) == normalize_whitespace(new_slot) {
                    *new_slot = old_line.to_string();
                }
            }
        }
    }
    new_lines.concat()
}

fn line_units(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

fn normalize_whitespace(line: &str) -> String {
    line.split_whitespace().collect()
}

fn cap_diff(rendered: String) -> String {
    if rendered.chars().count() > 6000 {
        let capped: String = rendered.chars().take(6000).collect();
        format!("{capped}\n\u{2026} (diff truncated)\n")
    } else {
        rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurable_context_lines_controls_unified_diff_radius() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nC\nd\ne\n";
        let diff = unified_diff_with_options(
            "x.txt",
            old,
            new,
            DiffOptions {
                context_lines: 1,
                ignore_whitespace: false,
            },
        );

        assert!(diff.contains(" b\n"), "diff: {diff}");
        assert!(diff.contains(" d\n"), "diff: {diff}");
        assert!(!diff.contains(" a\n"), "diff: {diff}");
        assert!(!diff.contains(" e\n"), "diff: {diff}");
    }

    #[test]
    fn ignore_whitespace_drops_paired_whitespace_only_changes() {
        let old = "fn main() {\n    run();\n    done();\n}\n";
        let new = "fn main() {\n  run();\n    finish();\n}\n";
        let diff = unified_diff_with_options(
            "x.rs",
            old,
            new,
            DiffOptions {
                context_lines: 3,
                ignore_whitespace: true,
            },
        );

        assert!(!diff.contains("-    run();\n"), "diff: {diff}");
        assert!(!diff.contains("+  run();\n"), "diff: {diff}");
        assert!(diff.contains("-    done();\n"), "diff: {diff}");
        assert!(diff.contains("+    finish();\n"), "diff: {diff}");
    }
}
