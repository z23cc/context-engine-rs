// `pub` (in the private `dispatch::editing::diff` module, so no external leak)
// so the gated `test-internals` re-export can reach it for the relocated
// fs-atomic dispatch integration tests, which call `DiffOptions::default()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    pub(in crate::dispatch) context_lines: usize,
    pub(in crate::dispatch) ignore_whitespace: bool,
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
