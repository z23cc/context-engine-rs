//! `replace` mode: content-addressed string replacement.
//!
//! Tries an exact substring match first (so within-line edits work), then falls
//! back to a whitespace-insensitive line-window match so indentation or internal
//! spacing drift does not block an otherwise-unambiguous edit.

use super::text::preview;
use super::{EditError, ReplaceEdit};

pub(super) fn apply(original: &str, edits: &[ReplaceEdit]) -> Result<String, EditError> {
    if edits.is_empty() {
        return Err(EditError::Empty);
    }
    let mut content = original.to_string();
    for edit in edits {
        content = apply_one(&content, edit)?;
    }
    Ok(content)
}

fn apply_one(content: &str, edit: &ReplaceEdit) -> Result<String, EditError> {
    if edit.old_text.is_empty() {
        return Err(EditError::Parse {
            mode: "replace",
            detail: "old_text must not be empty".to_string(),
        });
    }

    let exact: Vec<usize> = content
        .match_indices(&edit.old_text)
        .map(|(idx, _)| idx)
        .collect();
    if !exact.is_empty() {
        if exact.len() > 1 && !edit.all {
            return Err(EditError::Ambiguous {
                occurrences: exact.len(),
                snippet: preview(&edit.old_text),
            });
        }
        return Ok(if edit.all {
            content.replace(&edit.old_text, &edit.new_text)
        } else {
            content.replacen(&edit.old_text, &edit.new_text, 1)
        });
    }

    fuzzy_replace(content, edit)
}

fn fuzzy_replace(content: &str, edit: &ReplaceEdit) -> Result<String, EditError> {
    let lines: Vec<&str> = content.split('\n').collect();
    let old_lines: Vec<&str> = edit.old_text.split('\n').collect();
    let window = old_lines.len();
    if window == 0 || window > lines.len() {
        return Err(EditError::NotFound {
            snippet: preview(&edit.old_text),
        });
    }

    let normalize = |line: &str| line.split_whitespace().collect::<Vec<_>>().join(" ");
    let needle: Vec<String> = old_lines.iter().map(|line| normalize(line)).collect();
    let starts: Vec<usize> = (0..=lines.len() - window)
        .filter(|&start| {
            (0..window).all(|offset| normalize(lines[start + offset]) == needle[offset])
        })
        .collect();

    match starts.len() {
        0 => {
            return Err(EditError::NotFound {
                snippet: preview(&edit.old_text),
            });
        }
        count if count > 1 && !edit.all => {
            return Err(EditError::Ambiguous {
                occurrences: count,
                snippet: preview(&edit.old_text),
            });
        }
        _ => {}
    }

    let new_lines: Vec<&str> = edit.new_text.split('\n').collect();
    let selected: std::collections::HashSet<usize> = if edit.all {
        starts.iter().copied().collect()
    } else {
        starts.iter().copied().take(1).collect()
    };

    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        if selected.contains(&index) {
            out.extend(new_lines.iter().copied());
            index += window;
        } else {
            out.push(lines[index]);
            index += 1;
        }
    }
    Ok(out.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str, all: bool) -> ReplaceEdit {
        ReplaceEdit {
            old_text: old.to_string(),
            new_text: new.to_string(),
            all,
        }
    }

    #[test]
    fn exact_unique_replace() {
        let out = apply(
            "let a = 1;\nlet b = 2;\n",
            &[edit("let a = 1;", "let a = 10;", false)],
        )
        .expect("replace");
        assert_eq!(out, "let a = 10;\nlet b = 2;\n");
    }

    #[test]
    fn within_line_replace() {
        let out = apply(
            "value = foo(bar);\n",
            &[edit("foo(bar)", "foo(baz)", false)],
        )
        .expect("replace");
        assert_eq!(out, "value = foo(baz);\n");
    }

    #[test]
    fn ambiguous_without_all_errors() {
        let err = apply("x = 1\nx = 1\n", &[edit("x = 1", "x = 2", false)]).expect_err("ambiguous");
        assert!(matches!(err, EditError::Ambiguous { occurrences: 2, .. }));
    }

    #[test]
    fn all_replaces_every_occurrence() {
        let out = apply("x = 1\nx = 1\n", &[edit("x = 1", "x = 2", true)]).expect("replace all");
        assert_eq!(out, "x = 2\nx = 2\n");
    }

    #[test]
    fn fuzzy_matches_internal_whitespace() {
        // exact "def f( ):" is absent (source has two spaces); fuzzy still hits.
        let out = apply(
            "def  f( ):\n    pass\n",
            &[edit("def f( ):", "def g():", false)],
        )
        .expect("fuzzy replace");
        assert_eq!(out, "def g():\n    pass\n");
    }

    #[test]
    fn missing_text_errors() {
        let err = apply("alpha\n", &[edit("beta", "gamma", false)]).expect_err("not found");
        assert!(matches!(err, EditError::NotFound { .. }));
    }
}
