use super::{DispatchError, ToolText, edit, preflight_changes};
use crate::{
    CatalogProvider,
    edit::FileChange,
    selection_rebase::{
        SelectionMutation, remove_selection, selected_key, transfer_selection,
        update_selection_for_content,
    },
};
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) selection: Option<SelectionMutation>,
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
            selection: None,
        }
    }

    fn delete(action: &'static str, path: String, selection: Option<SelectionMutation>) -> Self {
        Self {
            action,
            path,
            moved_to: None,
            tag: None,
            view: None,
            diff: None,
            diagnostics: Vec::new(),
            selection,
        }
    }

    fn moved(
        action: &'static str,
        from: String,
        to: String,
        selection: Option<SelectionMutation>,
    ) -> Self {
        Self {
            action,
            path: from,
            moved_to: Some(to),
            tag: None,
            view: None,
            diff: None,
            diagnostics: Vec::new(),
            selection,
        }
    }

    fn with_selection(mut self, selection: Option<SelectionMutation>) -> Self {
        self.selection = selection;
        self
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
        push_selection_summary(&mut out, &self.files);
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

fn push_selection_summary(out: &mut String, files: &[EditedFile]) {
    let selections: Vec<&SelectionMutation> = files
        .iter()
        .filter_map(|file| file.selection.as_ref())
        .collect();
    if selections.is_empty() {
        return;
    }
    let rebased: usize = selections
        .iter()
        .filter(|item| item.mode_before == "slices" && item.mode_after.as_deref() == Some("slices"))
        .map(|item| item.ranges_after.len())
        .sum();
    let dropped: usize = selections.iter().map(|item| item.dropped.len()).sum();
    let removed = selections
        .iter()
        .filter(|item| item.mode_after.is_none())
        .count();
    let moved = selections
        .iter()
        .filter(|item| {
            item.new_path
                .as_deref()
                .is_some_and(|new_path| new_path != item.old_path)
        })
        .count();
    out.push_str(&format!(
        "selection: rebased {rebased} slice(s), dropped {dropped}, removed {removed}, moved {moved}\n"
    ));
}

pub(super) fn apply_changes<P: CatalogProvider + ?Sized>(
    provider: &P,
    changes: Vec<FileChange>,
    diff_options: DiffOptions,
    atomic: bool,
) -> Result<EditResult, DispatchError> {
    preflight_changes(provider, &changes)?;
    if atomic {
        return apply_atomic_changes(provider, changes, diff_options);
    }
    let mut files = Vec::with_capacity(changes.len());
    for change in changes {
        let edited = match change {
            FileChange::Create { path, content } => {
                provider.write_text(Path::new(&path), &content)?;
                EditedFile::with_content("create", path, None, &content, "", diff_options)
            }
            FileChange::Update { path, content } => {
                apply_update(provider, "update", path, content, diff_options)?
            }
            FileChange::Delete { path } => apply_delete_file(provider, path)?,
            FileChange::Rename { from, to, content } => {
                apply_rename(provider, "rename", from, to, content, diff_options)?
            }
        };
        files.push(edited);
    }
    Ok(EditResult { files })
}

struct AtomicPlan {
    change: FileChange,
    old: String,
    old_destination: String,
    before_key: Option<crate::selection::SelectionKey>,
    destination_key: Option<crate::selection::SelectionKey>,
}

fn apply_atomic_changes<P: CatalogProvider + ?Sized>(
    provider: &P,
    changes: Vec<FileChange>,
    diff_options: DiffOptions,
) -> Result<EditResult, DispatchError> {
    let plans = collect_atomic_plans(provider, &changes);
    provider.apply_file_batch(&changes, true)?;
    let files = plans
        .into_iter()
        .map(|plan| finalize_atomic_plan(provider, plan, diff_options))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(EditResult { files })
}

fn collect_atomic_plans<P: CatalogProvider + ?Sized>(
    provider: &P,
    changes: &[FileChange],
) -> Vec<AtomicPlan> {
    changes
        .iter()
        .cloned()
        .map(|change| {
            let (path, destination) = change_paths(&change);
            AtomicPlan {
                old: path.map_or_else(String::new, |path| read_old(provider, path)),
                old_destination: destination
                    .map_or_else(String::new, |path| read_old(provider, path)),
                before_key: path.and_then(|path| selected_key(provider, path)),
                destination_key: destination.and_then(|path| selected_key(provider, path)),
                change,
            }
        })
        .collect()
}

fn change_paths(change: &FileChange) -> (Option<&str>, Option<&str>) {
    match change {
        FileChange::Create { path, .. } => (None, Some(path)),
        FileChange::Update { path, .. } | FileChange::Delete { path } => (Some(path), None),
        FileChange::Rename { from, to, .. } => (Some(from), Some(to)),
    }
}

fn finalize_atomic_plan<P: CatalogProvider + ?Sized>(
    provider: &P,
    plan: AtomicPlan,
    diff_options: DiffOptions,
) -> Result<EditedFile, DispatchError> {
    match plan.change.clone() {
        FileChange::Create { path, content } => Ok(EditedFile::with_content(
            "create",
            path,
            None,
            &content,
            "",
            diff_options,
        )),
        FileChange::Update { path, content } => Ok(finalize_atomic_update(
            provider,
            path,
            content,
            plan.old,
            plan.before_key,
            diff_options,
        )),
        FileChange::Delete { path } => {
            let selection = remove_selection(provider, plan.before_key);
            Ok(EditedFile::delete("delete", path, selection))
        }
        FileChange::Rename { from, to, content } => Ok(finalize_atomic_rename(
            provider,
            from,
            to,
            content,
            plan,
            diff_options,
        )),
    }
}

fn finalize_atomic_update<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: String,
    content: String,
    old: String,
    before_key: Option<crate::selection::SelectionKey>,
    diff_options: DiffOptions,
) -> EditedFile {
    let after_key = selected_key(provider, &path);
    let selection = update_selection_for_content(
        provider, before_key, after_key, &path, &path, &old, &content,
    );
    EditedFile::with_content("update", path, None, &content, &old, diff_options)
        .with_selection(selection)
}

fn finalize_atomic_rename<P: CatalogProvider + ?Sized>(
    provider: &P,
    from: String,
    to: String,
    content: String,
    plan: AtomicPlan,
    diff_options: DiffOptions,
) -> EditedFile {
    let after_key = selected_key(provider, &to);
    let selection = update_selection_for_content(
        provider,
        plan.before_key,
        after_key.clone(),
        &from,
        &to,
        &plan.old,
        &content,
    )
    .or_else(|| {
        update_selection_for_content(
            provider,
            plan.destination_key,
            after_key,
            &to,
            &to,
            &plan.old_destination,
            &content,
        )
    });
    EditedFile::with_content("rename", from, Some(to), &content, &plan.old, diff_options)
        .with_selection(selection)
}

pub(super) fn apply_write<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: String,
    content: String,
) -> Result<EditResult, DispatchError> {
    Ok(EditResult {
        files: vec![apply_update(
            provider,
            "write",
            path,
            content,
            DiffOptions::default(),
        )?],
    })
}

pub(super) fn apply_delete<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: String,
) -> Result<EditResult, DispatchError> {
    let before_key = selected_key(provider, &path);
    provider.delete_file(Path::new(&path))?;
    let selection = remove_selection(provider, before_key);
    Ok(EditResult {
        files: vec![EditedFile::delete("delete", path, selection)],
    })
}

fn apply_delete_file<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: String,
) -> Result<EditedFile, DispatchError> {
    let before_key = selected_key(provider, &path);
    provider.delete_file(Path::new(&path))?;
    let selection = remove_selection(provider, before_key);
    Ok(EditedFile::delete("delete", path, selection))
}

pub(super) fn apply_move<P: CatalogProvider + ?Sized>(
    provider: &P,
    from: String,
    to: String,
) -> Result<EditResult, DispatchError> {
    let old = read_old(provider, &from);
    let old_destination = read_old(provider, &to);
    let before_key = selected_key(provider, &from);
    let destination_key = selected_key(provider, &to);
    provider.rename_file(Path::new(&from), Path::new(&to))?;
    let after_key = selected_key(provider, &to);
    let selection = transfer_selection(provider, before_key, after_key.clone(), &from, &to)
        .or_else(|| {
            update_selection_for_content(
                provider,
                destination_key,
                after_key,
                &to,
                &to,
                &old_destination,
                &old,
            )
        });
    Ok(EditResult {
        files: vec![EditedFile::moved("move", from, to, selection)],
    })
}

pub(super) fn apply_content_update_with_old<P: CatalogProvider + ?Sized>(
    provider: &P,
    action: &'static str,
    path: String,
    content: String,
    old: String,
    diff_options: DiffOptions,
) -> Result<EditResult, DispatchError> {
    Ok(EditResult {
        files: vec![write_content_update(
            provider,
            action,
            path,
            None,
            content,
            old,
            diff_options,
        )?],
    })
}

fn apply_update<P: CatalogProvider + ?Sized>(
    provider: &P,
    action: &'static str,
    path: String,
    content: String,
    diff_options: DiffOptions,
) -> Result<EditedFile, DispatchError> {
    let old = read_old(provider, &path);
    write_content_update(provider, action, path, None, content, old, diff_options)
}

fn apply_rename<P: CatalogProvider + ?Sized>(
    provider: &P,
    action: &'static str,
    from: String,
    to: String,
    content: String,
    diff_options: DiffOptions,
) -> Result<EditedFile, DispatchError> {
    let old = read_old(provider, &from);
    let old_destination = read_old(provider, &to);
    let before_key = selected_key(provider, &from);
    let destination_key = selected_key(provider, &to);
    provider.rename_file(Path::new(&from), Path::new(&to))?;
    provider.write_text(Path::new(&to), &content)?;
    let after_key = selected_key(provider, &to);
    let selection = update_selection_for_content(
        provider,
        before_key,
        after_key.clone(),
        &from,
        &to,
        &old,
        &content,
    )
    .or_else(|| {
        update_selection_for_content(
            provider,
            destination_key,
            after_key,
            &to,
            &to,
            &old_destination,
            &content,
        )
    });
    Ok(
        EditedFile::with_content(action, from, Some(to), &content, &old, diff_options)
            .with_selection(selection),
    )
}

fn write_content_update<P: CatalogProvider + ?Sized>(
    provider: &P,
    action: &'static str,
    path: String,
    moved_to: Option<String>,
    content: String,
    old: String,
    diff_options: DiffOptions,
) -> Result<EditedFile, DispatchError> {
    let before_key = selected_key(provider, &path);
    provider.write_text(Path::new(&path), &content)?;
    let after_path = moved_to.as_deref().unwrap_or(&path);
    let after_key = selected_key(provider, after_path);
    let selection = update_selection_for_content(
        provider, before_key, after_key, &path, after_path, &old, &content,
    );
    Ok(
        EditedFile::with_content(action, path, moved_to, &content, &old, diff_options)
            .with_selection(selection),
    )
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
