use std::ops::Range;

use super::language::Language;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarkdownFence {
    pub(super) language: Language,
    pub(super) body: Range<usize>,
    indent: usize,
}

pub(super) fn supported_fences(lines: &[&str]) -> Vec<MarkdownFence> {
    let mut fences = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let Some(opening) = opening_fence(lines[index]) else {
            index += 1;
            continue;
        };
        let body_start = index + 1;
        index = body_start;
        while index < lines.len() && !closing_fence(lines[index], opening.marker, opening.min_len) {
            index += 1;
        }
        if let Some(language) = opening.language {
            fences.push(MarkdownFence {
                language,
                body: body_start..index,
                indent: opening.indent,
            });
        }
        index += usize::from(index < lines.len());
    }
    fences
}

pub(super) fn fence_source(lines: &[&str], fence: &MarkdownFence) -> String {
    lines[fence.body.clone()]
        .iter()
        .map(|line| strip_fence_indent(line, fence.indent))
        .collect()
}

pub(super) fn fence_column_offsets(lines: &[&str], fence: &MarkdownFence) -> Vec<usize> {
    lines[fence.body.clone()]
        .iter()
        .map(|line| removable_fence_indent(line, fence.indent))
        .collect()
}

pub(super) fn is_markdown_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some("md" | "markdown" | "mdown" | "mkdn" | "mdx")
    )
}

#[derive(Debug, Clone, Copy)]
struct OpeningFence {
    marker: char,
    min_len: usize,
    indent: usize,
    language: Option<Language>,
}

fn opening_fence(line: &str) -> Option<OpeningFence> {
    let (indent, trimmed) = commonmark_fence_candidate(line)?;
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let count = trimmed.chars().take_while(|ch| *ch == marker).count();
    if count < 3 {
        return None;
    }
    let info = trimmed.get(count..)?.trim();
    Some(OpeningFence {
        marker,
        min_len: count,
        indent,
        language: Language::from_fence_info(info),
    })
}

fn closing_fence(line: &str, marker: char, min_len: usize) -> bool {
    let Some((_, trimmed)) = commonmark_fence_candidate(line) else {
        return false;
    };
    let count = trimmed.chars().take_while(|ch| *ch == marker).count();
    count >= min_len
        && trimmed
            .get(count..)
            .is_some_and(|rest| rest.trim().is_empty())
}

fn commonmark_fence_candidate(line: &str) -> Option<(usize, &str)> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    (indent <= 3)
        .then(|| line.get(indent..).map(|trimmed| (indent, trimmed)))
        .flatten()
}

fn strip_fence_indent(line: &str, indent: usize) -> &str {
    let removable = removable_fence_indent(line, indent);
    line.get(removable..).unwrap_or(line)
}

fn removable_fence_indent(line: &str, indent: usize) -> usize {
    line.bytes()
        .take_while(|byte| *byte == b' ')
        .count()
        .min(indent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_supported_fences_and_skips_indented_code_blocks() {
        let lines = [
            "    ```rust\n",
            "    fn ignored() {}\n",
            "    ```\n",
            "   ```rs\n",
            "pub fn accepted() {}\n",
            "   ```\n",
        ];

        let fences = supported_fences(&lines);

        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].language.name(), "rust");
        assert_eq!(fences[0].body, 4..5);
    }

    #[test]
    fn unsupported_outer_fence_suppresses_inner_supported_fence() {
        let lines = [
            "```text\n",
            "literal prose\n",
            "```rust\n",
            "fn ignored() {}\n",
            "```\n",
            "```\n",
        ];

        let fences = supported_fences(&lines);

        assert!(fences.is_empty());
    }

    #[test]
    fn unterminated_supported_fence_runs_to_eof() {
        let lines = ["intro\n", "```python\n", "def loose():\n", "    return 1\n"];

        let fences = supported_fences(&lines);

        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].language.name(), "python");
        assert_eq!(fences[0].body, 2..4);
    }
}
