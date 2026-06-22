use crate::selection::LineRange;

pub(super) fn range_label(range: &LineRange) -> String {
    range
        .label
        .as_deref()
        .map(sanitize_label)
        .filter(|label| !label.trim().is_empty())
        .unwrap_or_else(|| format!("lines {}-{}", range.start_line, range.end_line))
}

pub(super) fn escape_attr(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn sanitize_label(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
        .map(|ch| {
            if matches!(ch, '\n' | '\r' | '\t') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_label_sanitizes_control_text() {
        let label = range_label(&LineRange::with_label(1, 2, "a&\"<b>\n\t\u{0007}c"));
        assert_eq!(label, "a&\"<b>  c");
        assert_eq!(escape_attr(&label), "a&amp;&quot;&lt;b&gt;  c");
    }
}
