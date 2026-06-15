//! `hashline` mode: line-anchored ops bound to a content hash.
//!
//! Each file section starts with `[PATH#HASH]`, where `HASH` is the 4-hex
//! content tag of the file the model edited. The patcher recomputes the tag of
//! the live file and refuses the edit ([`EditError::StaleHash`]) if it differs,
//! so a stale edit can never silently corrupt a file.
//!
//! ```text
//! *** Begin Patch
//! [src/lib.rs#1A2B]
//! SWAP 1.=1:          replace lines 1..=1 with the body rows
//! +const X = 1;
//! INS.POST 3:         insert body rows after line 3
//! +// added
//! DEL 5.=6            delete lines 5..=6 (no body)
//! INS.HEAD:           / INS.TAIL: / INS.PRE A:
//! SWAP.BLK A: / DEL.BLK A / INS.BLK.POST A:   block-level (heuristic span)
//! *** End Patch
//! ```
//!
//! Body rows are `+TEXT` (a bare `+` is a blank line). Ops in a section all
//! reference original line numbers; they are applied bottom-up so earlier edits
//! do not shift later ones.

use super::text::{self, preview};
use super::{EditError, FileChange, FileReader};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";

#[derive(Debug)]
enum Op {
    Replace {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    Delete {
        start: usize,
        end: usize,
    },
    InsertBefore {
        line: usize,
        body: Vec<String>,
    },
    InsertAfter {
        line: usize,
        body: Vec<String>,
    },
    InsertHead {
        body: Vec<String>,
    },
    InsertTail {
        body: Vec<String>,
    },
    ReplaceBlock {
        start: usize,
        body: Vec<String>,
    },
    DeleteBlock {
        start: usize,
    },
    InsertBlockAfter {
        start: usize,
        body: Vec<String>,
    },
}

pub(super) fn plan(text: &str, reader: &impl FileReader) -> Result<Vec<FileChange>, EditError> {
    let normalized = text::normalize(text);
    let lines: Vec<&str> = normalized.split('\n').collect();

    let mut index = 0;
    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }
    if index >= lines.len() || lines[index] != BEGIN {
        return Err(parse_err(format!("patch must start with `{BEGIN}`")));
    }
    index += 1;

    let mut changes = Vec::new();
    let mut closed = false;
    while index < lines.len() {
        let line = lines[index];
        if line == END {
            closed = true;
            break;
        }
        if line.trim().is_empty() {
            index += 1;
            continue;
        }

        let (path, expected) = parse_header(line)
            .ok_or_else(|| parse_err(format!("expected `[PATH#HASH]`, got {}", preview(line))))?;
        index += 1;

        let original = reader
            .read_text(&path)
            .ok_or_else(|| EditError::MissingFile(path.clone()))?;
        let newline = text::detect_newline(&original);
        let normalized_original = text::normalize(&original);
        let actual = text::content_hash(&normalized_original);
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(EditError::StaleHash {
                path,
                expected,
                actual,
            });
        }

        let mut ops = Vec::new();
        while index < lines.len() && lines[index] != END && !lines[index].starts_with('[') {
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            ops.push(parse_op(&lines, &mut index)?);
        }
        if ops.is_empty() {
            return Err(parse_err(format!("section for {path} has no operations")));
        }
        let updated = apply_ops(&path, &normalized_original, &ops)?;
        changes.push(FileChange::Update {
            path,
            content: text::restore_newline(&updated, newline),
        });
    }

    if !closed {
        return Err(parse_err(format!("patch is missing `{END}`")));
    }
    if changes.is_empty() {
        return Err(EditError::Empty);
    }
    Ok(changes)
}

fn parse_err(detail: String) -> EditError {
    EditError::Parse {
        mode: "hashline",
        detail,
    }
}

fn parse_header(line: &str) -> Option<(String, String)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (path, hash) = inner.rsplit_once('#')?;
    if path.is_empty() || hash.len() != 4 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((path.to_string(), hash.to_string()))
}

fn parse_op(lines: &[&str], index: &mut usize) -> Result<Op, EditError> {
    let directive = lines[*index];
    *index += 1;

    if let Some(rest) = directive.strip_prefix("SWAP.BLK ") {
        let start = parse_lid(strip_colon(rest)?)?;
        return Ok(Op::ReplaceBlock {
            start,
            body: collect_body(lines, index),
        });
    }
    if let Some(rest) = directive.strip_prefix("SWAP ") {
        let (start, end) = parse_range(strip_colon(rest)?)?;
        return Ok(Op::Replace {
            start,
            end,
            body: collect_body(lines, index),
        });
    }
    if let Some(rest) = directive.strip_prefix("INS.BLK.POST ") {
        let start = parse_lid(strip_colon(rest)?)?;
        return Ok(Op::InsertBlockAfter {
            start,
            body: collect_body(lines, index),
        });
    }
    if let Some(rest) = directive.strip_prefix("INS.PRE ") {
        let line = parse_lid(strip_colon(rest)?)?;
        return Ok(Op::InsertBefore {
            line,
            body: collect_body(lines, index),
        });
    }
    if let Some(rest) = directive.strip_prefix("INS.POST ") {
        let line = parse_lid(strip_colon(rest)?)?;
        return Ok(Op::InsertAfter {
            line,
            body: collect_body(lines, index),
        });
    }
    if directive == "INS.HEAD:" {
        return Ok(Op::InsertHead {
            body: collect_body(lines, index),
        });
    }
    if directive == "INS.TAIL:" {
        return Ok(Op::InsertTail {
            body: collect_body(lines, index),
        });
    }
    if let Some(rest) = directive.strip_prefix("DEL.BLK ") {
        return Ok(Op::DeleteBlock {
            start: parse_lid(rest.trim())?,
        });
    }
    if let Some(rest) = directive.strip_prefix("DEL ") {
        let (start, end) = parse_range(rest.trim())?;
        return Ok(Op::Delete { start, end });
    }
    Err(parse_err(format!(
        "unknown hashline op: {}",
        preview(directive)
    )))
}

fn strip_colon(rest: &str) -> Result<&str, EditError> {
    rest.strip_suffix(':')
        .ok_or_else(|| parse_err(format!("op must end with ':' — got {}", preview(rest))))
}

fn parse_lid(value: &str) -> Result<usize, EditError> {
    let value = value.trim();
    let parsed: usize = value
        .parse()
        .map_err(|_| parse_err(format!("invalid line number: {}", preview(value))))?;
    if parsed == 0 {
        return Err(parse_err("line numbers are 1-based".to_string()));
    }
    Ok(parsed)
}

fn parse_range(value: &str) -> Result<(usize, usize), EditError> {
    let (start, end) = value
        .trim()
        .split_once(".=")
        .ok_or_else(|| parse_err(format!("expected range `A.=B`, got {}", preview(value))))?;
    Ok((parse_lid(start)?, parse_lid(end)?))
}

fn collect_body(lines: &[&str], index: &mut usize) -> Vec<String> {
    let mut body = Vec::new();
    while *index < lines.len() && lines[*index].starts_with('+') {
        body.push(lines[*index][1..].to_string());
        *index += 1;
    }
    body
}

struct Splice {
    at: usize,
    remove: usize,
    body: Vec<String>,
}

fn apply_ops(path: &str, original: &str, ops: &[Op]) -> Result<String, EditError> {
    let lines: Vec<&str> = original.split('\n').collect();
    let total = lines.len();
    let tail_pos = if lines.last() == Some(&"") {
        total - 1
    } else {
        total
    };

    let mut splices = Vec::with_capacity(ops.len());
    for op in ops {
        splices.push(to_splice(op, path, original, &lines, total, tail_pos)?);
    }

    // Reject overlapping range edits — line numbers all reference the original,
    // so an overlap would mean two ops fighting over the same lines.
    let mut ranges: Vec<(usize, usize)> = splices
        .iter()
        .filter(|splice| splice.remove > 0)
        .map(|splice| (splice.at, splice.at + splice.remove))
        .collect();
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        if pair[1].0 < pair[0].1 {
            return Err(parse_err(
                "overlapping operations on the same lines".to_string(),
            ));
        }
    }

    // Apply bottom-up so original line numbers stay valid as we splice.
    splices.sort_by_key(|splice| std::cmp::Reverse(splice.at));
    let mut out: Vec<String> = lines.iter().map(|line| (*line).to_string()).collect();
    for splice in splices {
        let end = splice.at + splice.remove;
        let tail = out.split_off(end);
        out.truncate(splice.at);
        out.extend(splice.body);
        out.extend(tail);
    }
    Ok(out.join("\n"))
}

fn to_splice(
    op: &Op,
    path: &str,
    original: &str,
    lines: &[&str],
    total: usize,
    tail_pos: usize,
) -> Result<Splice, EditError> {
    let body = |rows: &[String]| rows.to_vec();
    match op {
        Op::Replace {
            start,
            end,
            body: b,
        } => {
            check_range(*start, *end, total)?;
            Ok(Splice {
                at: start - 1,
                remove: end - start + 1,
                body: body(b),
            })
        }
        Op::Delete { start, end } => {
            check_range(*start, *end, total)?;
            Ok(Splice {
                at: start - 1,
                remove: end - start + 1,
                body: Vec::new(),
            })
        }
        Op::InsertBefore { line, body: b } => {
            check_line(*line, total)?;
            Ok(Splice {
                at: line - 1,
                remove: 0,
                body: body(b),
            })
        }
        Op::InsertAfter { line, body: b } => {
            check_line(*line, total)?;
            Ok(Splice {
                at: *line,
                remove: 0,
                body: body(b),
            })
        }
        Op::InsertHead { body: b } => Ok(Splice {
            at: 0,
            remove: 0,
            body: body(b),
        }),
        Op::InsertTail { body: b } => Ok(Splice {
            at: tail_pos,
            remove: 0,
            body: body(b),
        }),
        Op::ReplaceBlock { start, body: b } => {
            let (block_start, block_end) = resolve_block(path, original, lines, *start)?;
            Ok(Splice {
                at: block_start - 1,
                remove: block_end - block_start + 1,
                body: body(b),
            })
        }
        Op::DeleteBlock { start } => {
            let (block_start, block_end) = resolve_block(path, original, lines, *start)?;
            Ok(Splice {
                at: block_start - 1,
                remove: block_end - block_start + 1,
                body: Vec::new(),
            })
        }
        Op::InsertBlockAfter { start, body: b } => {
            let (_, block_end) = resolve_block(path, original, lines, *start)?;
            Ok(Splice {
                at: block_end,
                remove: 0,
                body: body(b),
            })
        }
    }
}

fn check_range(start: usize, end: usize, total: usize) -> Result<(), EditError> {
    if start == 0 || end < start || end > total {
        return Err(EditError::LineOutOfRange {
            line: if end > total { end } else { start },
            total,
        });
    }
    Ok(())
}

fn check_line(line: usize, total: usize) -> Result<(), EditError> {
    if line == 0 || line > total {
        return Err(EditError::LineOutOfRange { line, total });
    }
    Ok(())
}

/// Resolve the block starting at 1-based `start` to an inclusive `(start, end)`
/// range. Uses tree-sitter for supported languages; otherwise falls back to a
/// brace-balance heuristic (start line opens an unclosed `{`) or the run of
/// more-indented following lines.
fn resolve_block(
    path: &str,
    original: &str,
    lines: &[&str],
    start: usize,
) -> Result<(usize, usize), EditError> {
    let total = lines.len();
    if start == 0 || start > total {
        return Err(EditError::LineOutOfRange { line: start, total });
    }
    // Prefer an exact tree-sitter block; fall back to the brace/indent heuristic
    // for unsupported languages or unparseable spans.
    if let Some((block_start, block_end)) = crate::codemap::block_span(path, original, start) {
        return Ok((block_start, block_end.clamp(block_start, total)));
    }
    let first = lines[start - 1];
    let net_braces = count(first, '{') - count(first, '}');
    if net_braces > 0 {
        let mut depth = net_braces;
        let mut cursor = start; // 0-based index of next line
        while cursor < total && depth > 0 {
            depth += count(lines[cursor], '{') - count(lines[cursor], '}');
            cursor += 1;
        }
        return Ok((start, cursor));
    }

    let indent = |line: &str| line.len() - line.trim_start().len();
    let base = indent(first);
    let mut end = start; // 1-based; at least the start line
    let mut cursor = start; // 0-based next line
    while cursor < total {
        let line = lines[cursor];
        if line.trim().is_empty() {
            cursor += 1;
            continue;
        }
        if indent(line) > base {
            end = cursor + 1;
            cursor += 1;
        } else {
            break;
        }
    }
    Ok((start, end))
}

fn count(line: &str, ch: char) -> i32 {
    line.matches(ch).count() as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
            .collect()
    }

    fn tag(content: &str) -> String {
        crate::edit::text::content_hash(&crate::edit::text::normalize(content))
    }

    fn run(file: &str, content: &str, ops: &str) -> Result<String, EditError> {
        let reader = files(&[(file, content)]);
        let patch = format!("{BEGIN}\n[{file}#{}]\n{ops}\n{END}\n", tag(content));
        let changes = plan(&patch, &reader)?;
        match changes.into_iter().next() {
            Some(FileChange::Update { content, .. }) => Ok(content),
            other => panic!("expected one Update, got {other:?}"),
        }
    }

    #[test]
    fn swap_replaces_line_range() {
        let out = run(
            "a.rs",
            "let a = 1;\nlet b = 2;\n",
            "SWAP 1.=1:\n+let a = 10;",
        )
        .expect("swap");
        assert_eq!(out, "let a = 10;\nlet b = 2;\n");
    }

    #[test]
    fn insert_post_and_pre() {
        let out = run("a.txt", "one\ntwo\n", "INS.POST 1:\n+after-one").expect("ins.post");
        assert_eq!(out, "one\nafter-one\ntwo\n");
        let out = run("a.txt", "one\ntwo\n", "INS.PRE 1:\n+before-one").expect("ins.pre");
        assert_eq!(out, "before-one\none\ntwo\n");
    }

    #[test]
    fn insert_head_and_tail() {
        let out = run("a.txt", "body\n", "INS.HEAD:\n+top").expect("head");
        assert_eq!(out, "top\nbody\n");
        let out = run("a.txt", "body\n", "INS.TAIL:\n+bottom").expect("tail");
        assert_eq!(out, "body\nbottom\n");
    }

    #[test]
    fn delete_range() {
        let out = run("a.txt", "a\nb\nc\n", "DEL 2.=2").expect("del");
        assert_eq!(out, "a\nc\n");
    }

    #[test]
    fn multiple_ops_use_original_line_numbers() {
        let out = run("a.txt", "a\nb\nc\n", "SWAP 1.=1:\n+A\nDEL 3.=3").expect("multi");
        assert_eq!(out, "A\nb\n");
    }

    #[test]
    fn stale_hash_is_rejected() {
        let reader = files(&[("a.txt", "current\n")]);
        let patch = format!("{BEGIN}\n[a.txt#0000]\nSWAP 1.=1:\n+x\n{END}\n");
        let err = plan(&patch, &reader).expect_err("stale");
        assert!(matches!(err, EditError::StaleHash { .. }));
    }

    #[test]
    fn block_swap_uses_brace_balance() {
        let content = "fn outer() {\n    inner();\n}\nlet keep = 1;\n";
        let out = run("a.rs", content, "SWAP.BLK 1:\n+fn outer() { done(); }").expect("blk");
        assert_eq!(out, "fn outer() { done(); }\nlet keep = 1;\n");
    }

    #[test]
    fn block_swap_uses_tree_sitter_past_brace_in_string() {
        // A `}` inside a string literal would fool brace-counting into ending the
        // block at line 2; tree-sitter resolves the real function span (1..=4).
        let content = "fn f() {\n    let s = \"}\";\n    g();\n}\nlet keep = 1;\n";
        let out = run("a.rs", content, "SWAP.BLK 1:\n+fn f() { done(); }").expect("blk");
        assert_eq!(out, "fn f() { done(); }\nlet keep = 1;\n");
    }

    #[test]
    fn out_of_range_line_errors() {
        let err = run("a.txt", "only\n", "SWAP 5.=5:\n+x").expect_err("range");
        assert!(matches!(err, EditError::LineOutOfRange { .. }));
    }
}
