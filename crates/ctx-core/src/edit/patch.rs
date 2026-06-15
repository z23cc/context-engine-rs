//! `patch` mode: anchored unified-diff hunks for a single file.
//!
//! Each hunk starts with `@@` (optionally `@@ ANCHOR`, an exact line or unique
//! substring copied from the file). Body lines are prefixed ` ` (context),
//! `-` (removed) or `+` (added). The removed+context lines locate the edit; an
//! optional anchor narrows the search so repeated context stays unambiguous.

use super::EditError;
use super::text::preview;

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Add(String),
    Remove(String),
}

#[derive(Debug)]
struct Hunk {
    anchor: Option<String>,
    lines: Vec<HunkLine>,
}

/// Apply all hunks in `diff` to `original` (LF-normalized), in order.
pub(super) fn apply_hunks(original: &str, diff: &str) -> Result<String, EditError> {
    let hunks = parse_hunks(diff)?;
    let mut lines: Vec<String> = original.split('\n').map(str::to_string).collect();
    for hunk in &hunks {
        apply_hunk(&mut lines, hunk)?;
    }
    Ok(lines.join("\n"))
}

fn parse_hunks(diff: &str) -> Result<Vec<Hunk>, EditError> {
    let parse_err = |detail: String| EditError::Parse {
        mode: "patch",
        detail,
    };
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;

    for raw in diff.split('\n') {
        if let Some(rest) = raw.strip_prefix("@@") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            let anchor = rest.trim();
            current = Some(Hunk {
                anchor: (!anchor.is_empty()).then(|| anchor.to_string()),
                lines: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = current.as_mut() else {
            if raw.trim().is_empty() {
                continue;
            }
            return Err(parse_err(
                "hunk body appears before any @@ header".to_string(),
            ));
        };
        match raw.chars().next() {
            Some(' ') => hunk.lines.push(HunkLine::Context(raw[1..].to_string())),
            Some('+') => hunk.lines.push(HunkLine::Add(raw[1..].to_string())),
            Some('-') => hunk.lines.push(HunkLine::Remove(raw[1..].to_string())),
            // A bare empty line is a blank context line authored without the
            // leading space; accept it rather than failing the whole patch.
            None => hunk.lines.push(HunkLine::Context(String::new())),
            Some(_) => {
                return Err(parse_err(format!("invalid hunk line: {}", preview(raw))));
            }
        }
    }
    if let Some(hunk) = current.take() {
        hunks.push(hunk);
    }

    if hunks.is_empty() {
        return Err(parse_err("diff contains no @@ hunks".to_string()));
    }
    for hunk in &hunks {
        let has_change = hunk
            .lines
            .iter()
            .any(|line| matches!(line, HunkLine::Add(_) | HunkLine::Remove(_)));
        if !has_change {
            return Err(parse_err("hunk has no +/- change".to_string()));
        }
    }
    Ok(hunks)
}

fn apply_hunk(lines: &mut Vec<String>, hunk: &Hunk) -> Result<(), EditError> {
    let before: Vec<&str> = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.as_str()),
            HunkLine::Add(_) => None,
        })
        .collect();
    let after: Vec<String> = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(text) | HunkLine::Add(text) => Some(text.clone()),
            HunkLine::Remove(_) => None,
        })
        .collect();

    if before.is_empty() {
        return Err(EditError::Parse {
            mode: "patch",
            detail: "hunk needs context or removed lines to anchor the change".to_string(),
        });
    }

    let from = match &hunk.anchor {
        Some(anchor) => lines
            .iter()
            .position(|line| line.contains(anchor.as_str()))
            .ok_or_else(|| EditError::ContextNotFound {
                snippet: preview(anchor),
            })?,
        None => 0,
    };

    let mut starts = find_matches(lines, &before, from, false);
    if starts.is_empty() {
        starts = find_matches(lines, &before, from, true);
    }
    match starts.as_slice() {
        [] => Err(EditError::ContextNotFound {
            snippet: preview(&before.join("\n")),
        }),
        [start] => {
            let start = *start;
            let mut out: Vec<String> = Vec::with_capacity(lines.len() + after.len());
            out.extend_from_slice(&lines[..start]);
            out.extend(after);
            out.extend_from_slice(&lines[start + before.len()..]);
            *lines = out;
            Ok(())
        }
        many => Err(EditError::Ambiguous {
            occurrences: many.len(),
            snippet: preview(&before.join("\n")),
        }),
    }
}

fn find_matches(lines: &[String], before: &[&str], from: usize, fuzzy: bool) -> Vec<usize> {
    if before.is_empty() || before.len() > lines.len() || from > lines.len() - before.len() {
        return Vec::new();
    }
    let eq = |a: &str, b: &str| if fuzzy { a.trim() == b.trim() } else { a == b };
    (from..=lines.len() - before.len())
        .filter(|&start| (0..before.len()).all(|offset| eq(&lines[start + offset], before[offset])))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_hunk_replaces_unique_context() {
        let original = "fn main() {\n    println!(\"hi\");\n}\n";
        let diff = "@@\n fn main() {\n-    println!(\"hi\");\n+    println!(\"hello\");\n }";
        let out = apply_hunks(original, diff).expect("patch");
        assert_eq!(out, "fn main() {\n    println!(\"hello\");\n}\n");
    }

    #[test]
    fn anchor_disambiguates_repeated_context() {
        let original = "fn a() {\n    work();\n}\nfn b() {\n    work();\n}\n";
        let diff = "@@ fn b()\n-    work();\n+    done();";
        let out = apply_hunks(original, diff).expect("patch");
        assert_eq!(out, "fn a() {\n    work();\n}\nfn b() {\n    done();\n}\n");
    }

    #[test]
    fn ambiguous_context_without_anchor_errors() {
        let original = "    work();\n    work();\n";
        let diff = "@@\n-    work();\n+    done();";
        let err = apply_hunks(original, diff).expect_err("ambiguous");
        assert!(matches!(err, EditError::Ambiguous { occurrences: 2, .. }));
    }

    #[test]
    fn missing_context_errors() {
        let err = apply_hunks("alpha\n", "@@\n-beta\n+gamma").expect_err("missing");
        assert!(matches!(err, EditError::ContextNotFound { .. }));
    }

    #[test]
    fn multiple_hunks_apply_in_order() {
        let original = "a\nb\nc\nd\n";
        let diff = "@@\n-a\n+A\n@@\n-d\n+D";
        let out = apply_hunks(original, diff).expect("patch");
        assert_eq!(out, "A\nb\nc\nD\n");
    }
}
