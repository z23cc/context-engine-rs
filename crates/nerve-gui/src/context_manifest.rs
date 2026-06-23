//! Selection manifest formatter for copying context working sets.

use crate::data::{SelectionFile, SelectionSummary};

pub(crate) fn handoff_text(
    summary: &SelectionSummary,
    workspace: &str,
    recipe: &str,
    context: &str,
) -> String {
    let context = if context.trim().is_empty() {
        "No assembled context yet. Build context before handing off."
    } else {
        context
    };
    format!(
        "# Context handoff\n\nWorkspace: {workspace}\nRecipe: {recipe}\nSelection: {} files · {} tokens\n\n{}\n\n## Assembled context\n\n{}",
        summary.files.len(),
        summary.total_tokens,
        manifest_text(summary),
        context
    )
}

pub(crate) fn manifest_text(summary: &SelectionSummary) -> String {
    if summary.files.is_empty() {
        return "# Selection manifest\n\nNo files selected.".into();
    }
    let mut out = format!(
        "# Selection manifest\n\n{} files · {} tokens\n\n",
        summary.files.len(),
        summary.total_tokens
    );
    for file in &summary.files {
        out.push_str(&format!("- {}", display_name(file)));
        out.push_str(&format!(" · {}", file.mode));
        if file.token_estimate > 0 {
            out.push_str(&format!(" · {} tok", file.token_estimate));
        }
        if !file.ranges.is_empty() {
            out.push_str(&format!(" · {}", range_summary(file)));
        }
        out.push('\n');
    }
    out
}

fn display_name(file: &SelectionFile) -> &str {
    if file.display_path.is_empty() {
        &file.path
    } else {
        &file.display_path
    }
}

fn range_summary(file: &SelectionFile) -> String {
    file.ranges
        .iter()
        .map(|range| {
            let label = range
                .label
                .as_ref()
                .map(|text| format!(" {text}"))
                .unwrap_or_default();
            format!("L{}-{}{}", range.start_line, range.end_line, label)
        })
        .collect::<Vec<_>>()
        .join(", ")
}
