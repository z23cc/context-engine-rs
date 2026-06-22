use crate::{
    CatalogEntry, CatalogProvider, LineRange, NerveError, Selection, SelectionMode,
    selection::SelectionKey,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildContextSensitiveFinding {
    pub path: String,
    pub display_path: String,
    pub line: usize,
    pub kind: String,
    pub severity: String,
    pub message: String,
}

#[derive(Clone, Copy)]
struct SensitiveRule {
    kind: &'static str,
    severity: &'static str,
    message: &'static str,
    matcher: fn(&str) -> bool,
}

const GENERIC_SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "auth_token",
    "client_secret",
    "password",
    "private_key",
    "secret",
    "secret_key",
    "token",
];

const RULES: &[SensitiveRule] = &[
    SensitiveRule {
        kind: "private_key_block",
        severity: "high",
        message: "private key material may be included in generated context",
        matcher: contains_private_key_block,
    },
    SensitiveRule {
        kind: "aws_access_key_id",
        severity: "high",
        message: "AWS access key id may be included in generated context",
        matcher: contains_aws_access_key,
    },
    SensitiveRule {
        kind: "github_token",
        severity: "high",
        message: "GitHub token may be included in generated context",
        matcher: contains_github_token,
    },
    SensitiveRule {
        kind: "openai_api_key",
        severity: "high",
        message: "OpenAI API key may be included in generated context",
        matcher: contains_openai_key,
    },
    SensitiveRule {
        kind: "slack_token",
        severity: "high",
        message: "Slack token may be included in generated context",
        matcher: contains_slack_token,
    },
    SensitiveRule {
        kind: "generic_secret_assignment",
        severity: "medium",
        message: "secret-like assignment may be included in generated context",
        matcher: contains_generic_secret_assignment,
    },
];

pub(super) fn scan_selection<P: CatalogProvider>(
    provider: &P,
    entries: &[CatalogEntry],
    selection: &Selection,
) -> Result<Vec<BuildContextSensitiveFinding>, NerveError> {
    let entries_by_key = entries
        .iter()
        .map(|entry| (selection_key(entry), entry))
        .collect::<BTreeMap<_, _>>();
    let mut findings = Vec::new();

    for (key, mode) in &selection.files {
        let Some(entry) = entries_by_key.get(key) else {
            continue;
        };
        match mode {
            SelectionMode::Full => scan_full_file(provider, entry, &mut findings)?,
            SelectionMode::Slices(ranges) => scan_slices(provider, entry, ranges, &mut findings)?,
            SelectionMode::CodemapOnly => scan_codemap(provider, entry, &mut findings)?,
        }
    }

    findings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line.cmp(&right.line))
            .then(left.kind.cmp(&right.kind))
    });
    findings.dedup();
    Ok(findings)
}

fn scan_full_file<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    findings: &mut Vec<BuildContextSensitiveFinding>,
) -> Result<(), NerveError> {
    let content = read_utf8(provider, entry)?;
    scan_text(provider, entry, 1, &content, findings);
    Ok(())
}

fn scan_slices<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    ranges: &[LineRange],
    findings: &mut Vec<BuildContextSensitiveFinding>,
) -> Result<(), NerveError> {
    let content = read_utf8(provider, entry)?;
    let key_blocks = private_key_blocks(&content);
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    for range in ranges {
        flag_intersecting_private_key_blocks(provider, entry, range, &key_blocks, findings);
        let start = range.start_line.max(1).min(lines.len().max(1));
        let end = range.end_line.max(start).min(lines.len().max(1));
        let slice = if lines.is_empty() {
            String::new()
        } else {
            lines[start - 1..end].concat()
        };
        scan_text(provider, entry, start, &slice, findings);
    }
    Ok(())
}

fn scan_codemap<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    findings: &mut Vec<BuildContextSensitiveFinding>,
) -> Result<(), NerveError> {
    let Ok(Some(parsed)) = provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? else {
        return Ok(());
    };
    for symbol in &parsed.symbols {
        if let Some(signature) = symbol.signature.as_deref() {
            scan_text(provider, entry, symbol.line, signature, findings);
        }
        for member in &symbol.members {
            if let Some(signature) = member.signature.as_deref() {
                scan_text(provider, entry, symbol.line, signature, findings);
            }
        }
    }
    Ok(())
}

fn read_utf8<P: CatalogProvider>(provider: &P, entry: &CatalogEntry) -> Result<String, NerveError> {
    let bytes = provider.read_bytes(&entry.abs_path)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn scan_text<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    start_line: usize,
    text: &str,
    findings: &mut Vec<BuildContextSensitiveFinding>,
) {
    for (offset, line) in text.lines().enumerate() {
        let line_number = start_line + offset;
        for rule in RULES {
            if (rule.matcher)(line) {
                push_finding(provider, entry, line_number, rule, findings);
            }
        }
    }
}

fn push_finding<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    line: usize,
    rule: &SensitiveRule,
    findings: &mut Vec<BuildContextSensitiveFinding>,
) {
    findings.push(BuildContextSensitiveFinding {
        path: entry.rel_path.clone(),
        display_path: provider.display_path(&entry.abs_path),
        line,
        kind: rule.kind.to_string(),
        severity: rule.severity.to_string(),
        message: rule.message.to_string(),
    });
}

fn flag_intersecting_private_key_blocks<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
    range: &LineRange,
    key_blocks: &[(usize, usize)],
    findings: &mut Vec<BuildContextSensitiveFinding>,
) {
    for (start, end) in key_blocks {
        if ranges_intersect(range.start_line, range.end_line, *start, *end) {
            push_finding(
                provider,
                entry,
                range.start_line.max(*start),
                &RULES[0],
                findings,
            );
        }
    }
}

fn private_key_blocks(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut open = None;
    for (idx, line) in lines.iter().enumerate() {
        let line_number = idx + 1;
        if open.is_none() && contains_private_key_block(line) {
            open = Some(line_number);
        }
        if open.is_some() && contains_private_key_end(line) {
            let start = open.take().expect("open block");
            blocks.push((start, line_number));
        }
    }
    if let Some(start) = open {
        blocks.push((start, lines.len().max(start)));
    }
    blocks
}

fn ranges_intersect(
    left_start: usize,
    left_end: usize,
    right_start: usize,
    right_end: usize,
) -> bool {
    left_start <= right_end && right_start <= left_end
}

fn contains_private_key_block(line: &str) -> bool {
    line.contains("-----BEGIN ") && line.contains(" PRIVATE KEY-----")
}

fn contains_private_key_end(line: &str) -> bool {
    line.contains("-----END ") && line.contains(" PRIVATE KEY-----")
}

fn contains_aws_access_key(line: &str) -> bool {
    contains_token_with_prefix(line, "AKIA", 20) || contains_token_with_prefix(line, "ASIA", 20)
}

fn contains_github_token(line: &str) -> bool {
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"]
        .iter()
        .any(|prefix| contains_token_with_prefix(line, prefix, prefix.len() + 20))
}

fn contains_openai_key(line: &str) -> bool {
    contains_token_with_prefix(line, "sk-", 24) || contains_token_with_prefix(line, "sk-proj-", 30)
}

fn contains_slack_token(line: &str) -> bool {
    ["xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-"]
        .iter()
        .any(|prefix| contains_token_with_prefix(line, prefix, prefix.len() + 12))
}

fn contains_generic_secret_assignment(line: &str) -> bool {
    let Some((key, value)) = split_assignment(line) else {
        return false;
    };
    let key = key
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .to_ascii_lowercase()
        .replace('-', "_");
    if !GENERIC_SECRET_KEYS
        .iter()
        .any(|candidate| key.ends_with(candidate))
    {
        return false;
    }
    let value = value.trim().trim_matches(['\'', '"', '`', ';', ',']);
    looks_secret_value(value)
}

fn split_assignment(line: &str) -> Option<(&str, &str)> {
    ["=", ":"]
        .iter()
        .filter_map(|separator| line.split_once(separator))
        .find(|(_, value)| !value.trim().is_empty())
}

fn looks_secret_value(value: &str) -> bool {
    value.len() >= 16
        && !value.contains(' ')
        && value.chars().any(|ch| ch.is_ascii_alphabetic())
        && value.chars().any(|ch| ch.is_ascii_digit())
}

fn contains_token_with_prefix(line: &str, prefix: &str, min_len: usize) -> bool {
    line.match_indices(prefix).any(|(idx, _)| {
        let token = line[idx..]
            .split(|ch: char| !(ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')))
            .next()
            .unwrap_or_default();
        token.len() >= min_len
    })
}

fn selection_key(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_private_key_body_when_only_body_slice_is_included() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("key.pem");
        std::fs::write(
            &path,
            "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\n-----END PRIVATE KEY-----\n",
        )
        .expect("write key");
        let provider = crate::FsCatalogProvider::new(
            crate::RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            crate::ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.rel_path == "key.pem")
            .expect("entry");
        let mut selection = Selection::default();
        selection.files.insert(
            selection_key(entry),
            SelectionMode::Slices(vec![LineRange::new(2, 2)]),
        );

        let findings = scan_selection(&provider, &snapshot.entries, &selection).expect("scan");

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, "private_key_block");
        assert_eq!(findings[0].line, 2);
    }

    #[test]
    fn scans_codemap_only_signatures_for_secret_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("client.py"),
            "def connect(token=\"sk-proj-1234567890abcdefghijklmnopqrstuvwxyz\"):\n    pass\n",
        )
        .expect("write client");
        let provider = crate::FsCatalogProvider::new(
            crate::RootPolicy::new(vec![dir.path().to_path_buf()]).expect("policy"),
            crate::ScanOptions::default(),
        );
        let snapshot = provider.snapshot().expect("snapshot");
        let entry = snapshot
            .entries
            .iter()
            .find(|entry| entry.rel_path == "client.py")
            .expect("entry");
        let mut selection = Selection::default();
        selection
            .files
            .insert(selection_key(entry), SelectionMode::CodemapOnly);

        let findings = scan_selection(&provider, &snapshot.entries, &selection).expect("scan");

        assert!(
            findings
                .iter()
                .any(|finding| finding.kind == "openai_api_key" && finding.line == 1),
            "{findings:?}"
        );
    }

    #[test]
    fn detects_high_confidence_tokens_without_revealing_values() {
        let mut findings = Vec::new();
        let entry = CatalogEntry {
            root_id: "root".to_string(),
            rel_path: "secrets.env".to_string(),
            abs_path: "secrets.env".into(),
            size: 0,
        };
        let provider = crate::MemoryCatalogProvider::default();

        scan_text(
            &provider,
            &entry,
            10,
            "OPENAI_API_KEY=sk-proj-1234567890abcdefghi\nAWS_ACCESS_KEY_ID=AKIA1234567890ABCDEF\n",
            &mut findings,
        );

        assert_eq!(findings.len(), 3);
        assert!(
            findings
                .iter()
                .any(|finding| finding.kind == "openai_api_key")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.kind == "aws_access_key_id")
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.kind == "generic_secret_assignment")
        );
        assert!(
            findings
                .iter()
                .all(|finding| !finding.message.contains("sk-proj"))
        );
        assert_eq!(findings[0].line, 10);
    }
}
