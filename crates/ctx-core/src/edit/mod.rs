//! Multi-mode file edit engine (pure, filesystem-agnostic).
//!
//! Four edit "languages", each suited to a different situation:
//! - [`EditRequest::Replace`]: fuzzy string search/replace, content-addressed.
//! - [`EditRequest::Patch`]: anchored unified-diff hunks for one file.
//! - [`EditRequest::ApplyPatch`]: the Codex `*** Begin Patch` multi-file envelope.
//! - [`EditRequest::Hashline`]: line-anchored ops bound to a content hash so a
//!   stale edit is rejected before it can corrupt a file.
//!
//! The engine is pure: current file contents are supplied through [`FileReader`]
//! and results are returned as [`FileChange`]s for the caller to persist. No
//! file is read from or written to disk here.

mod apply_patch;
mod hashline;
mod patch;
mod replace;
mod text;

use std::collections::BTreeMap;

/// Reads the current text of a file by project-relative path; `None` if absent.
pub trait FileReader {
    fn read_text(&self, path: &str) -> Option<String>;
}

impl FileReader for BTreeMap<String, String> {
    fn read_text(&self, path: &str) -> Option<String> {
        self.get(path).cloned()
    }
}

/// A change the caller must persist to realize a planned edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Create {
        path: String,
        content: String,
    },
    Update {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
        content: String,
    },
}

/// An edit-planning error. No file is mutated when one is returned.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    #[error("no edits supplied")]
    Empty,
    #[error("malformed {mode} edit: {detail}")]
    Parse { mode: &'static str, detail: String },
    #[error("text to replace was not found: {snippet}")]
    NotFound { snippet: String },
    #[error(
        "text to replace is ambiguous ({occurrences} matches); add context or set all=true: {snippet}"
    )]
    Ambiguous { occurrences: usize, snippet: String },
    #[error("context not found while applying hunk: {snippet}")]
    ContextNotFound { snippet: String },
    #[error("file not found: {0}")]
    MissingFile(String),
    #[error("file already exists: {0}")]
    FileExists(String),
    #[error("line {line} is out of range (file has {total} lines)")]
    LineOutOfRange { line: usize, total: usize },
    #[error(
        "stale edit for {path}: file hash is {actual} but the patch expected {expected}; re-read the file"
    )]
    StaleHash {
        path: String,
        expected: String,
        actual: String,
    },
}

/// One replacement for [`EditRequest::Replace`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReplaceEdit {
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub all: bool,
}

/// One operation for [`EditRequest::Patch`] against the request's top-level path.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum PatchEntry {
    /// Apply diff hunks in place, optionally renaming the file.
    Update {
        #[serde(default)]
        rename: Option<String>,
        diff: String,
    },
    /// Create a new file whose contents are `diff` verbatim (no line prefixes).
    Create { diff: String },
    /// Delete the file.
    Delete,
}

/// A planned edit in one of the four supported languages.
#[derive(Debug, Clone)]
pub enum EditRequest {
    /// Fuzzy string search/replace within a single file.
    Replace {
        path: String,
        edits: Vec<ReplaceEdit>,
    },
    /// Anchored unified-diff hunks (create/update/delete/rename) for one path.
    Patch {
        path: String,
        entries: Vec<PatchEntry>,
    },
    /// The Codex `*** Begin Patch` multi-file envelope.
    ApplyPatch { patch: String },
    /// The hashline `[PATH#HASH]` line-anchored format with stale-edit rejection.
    Hashline { patch: String },
}

/// Plan an edit against current file contents, returning changes to persist.
///
/// Pure: current contents come from `reader`; nothing is written. Multi-file
/// modes preflight every section so a partial batch never lands.
pub fn apply(
    request: &EditRequest,
    reader: &impl FileReader,
) -> Result<Vec<FileChange>, EditError> {
    match request {
        EditRequest::Replace { path, edits } => {
            let original = reader
                .read_text(path)
                .ok_or_else(|| EditError::MissingFile(path.clone()))?;
            let newline = text::detect_newline(&original);
            let updated = replace::apply(&text::normalize(&original), edits)?;
            Ok(vec![FileChange::Update {
                path: path.clone(),
                content: text::restore_newline(&updated, newline),
            }])
        }
        EditRequest::Patch { path, entries } => patch_dispatch(path, entries, reader),
        EditRequest::ApplyPatch { patch } => apply_patch::plan(patch, reader),
        EditRequest::Hashline { patch } => hashline::plan(patch, reader),
    }
}

/// Apply a sequence of [`PatchEntry`] operations to one evolving path.
fn patch_dispatch(
    path: &str,
    entries: &[PatchEntry],
    reader: &impl FileReader,
) -> Result<Vec<FileChange>, EditError> {
    if entries.is_empty() {
        return Err(EditError::Empty);
    }
    let mut changes = Vec::new();
    let mut current_path = path.to_string();
    let mut current: Option<(text::Newline, String)> = reader
        .read_text(&current_path)
        .map(|content| (text::detect_newline(&content), text::normalize(&content)));

    for entry in entries {
        match entry {
            PatchEntry::Create { diff } => {
                if current.is_some() {
                    return Err(EditError::FileExists(current_path.clone()));
                }
                let content = text::normalize(diff);
                changes.push(FileChange::Create {
                    path: current_path.clone(),
                    content: content.clone(),
                });
                current = Some((text::Newline::Lf, content));
            }
            PatchEntry::Delete => {
                if current.is_none() {
                    return Err(EditError::MissingFile(current_path.clone()));
                }
                changes.push(FileChange::Delete {
                    path: current_path.clone(),
                });
                current = None;
            }
            PatchEntry::Update { rename, diff } => {
                let (newline, content) = current
                    .clone()
                    .ok_or_else(|| EditError::MissingFile(current_path.clone()))?;
                let updated = patch::apply_hunks(&content, diff)?;
                let restored = text::restore_newline(&updated, newline);
                match rename {
                    Some(new_path) => {
                        changes.push(FileChange::Rename {
                            from: current_path.clone(),
                            to: new_path.clone(),
                            content: restored,
                        });
                        current_path = new_path.clone();
                    }
                    None => changes.push(FileChange::Update {
                        path: current_path.clone(),
                        content: restored,
                    }),
                }
                current = Some((newline, updated));
            }
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
            .collect()
    }

    #[test]
    fn replace_request_updates_file() {
        let reader = files(&[("a.rs", "fn main() {}\n")]);
        let request = EditRequest::Replace {
            path: "a.rs".to_string(),
            edits: vec![ReplaceEdit {
                old_text: "fn main() {}".to_string(),
                new_text: "fn main() { run(); }".to_string(),
                all: false,
            }],
        };
        let changes = apply(&request, &reader).expect("apply");
        assert_eq!(
            changes,
            vec![FileChange::Update {
                path: "a.rs".to_string(),
                content: "fn main() { run(); }\n".to_string(),
            }]
        );
    }

    #[test]
    fn replace_request_missing_file_errors() {
        let reader = files(&[]);
        let request = EditRequest::Replace {
            path: "nope.rs".to_string(),
            edits: vec![ReplaceEdit {
                old_text: "a".to_string(),
                new_text: "b".to_string(),
                all: false,
            }],
        };
        assert_eq!(
            apply(&request, &reader),
            Err(EditError::MissingFile("nope.rs".to_string()))
        );
    }

    #[test]
    fn crlf_newlines_are_preserved() {
        let reader = files(&[("w.txt", "alpha\r\nbeta\r\n")]);
        let request = EditRequest::Replace {
            path: "w.txt".to_string(),
            edits: vec![ReplaceEdit {
                old_text: "beta".to_string(),
                new_text: "gamma".to_string(),
                all: false,
            }],
        };
        let changes = apply(&request, &reader).expect("apply");
        assert_eq!(
            changes,
            vec![FileChange::Update {
                path: "w.txt".to_string(),
                content: "alpha\r\ngamma\r\n".to_string(),
            }]
        );
    }
}
