//! `apply-patch` mode: the Codex `*** Begin Patch` multi-file envelope.
//!
//! ```text
//! *** Begin Patch
//! *** Add File: path        (then one or more `+`-prefixed content lines)
//! *** Delete File: path
//! *** Update File: path
//! *** Move to: new/path     (optional, immediately after Update File)
//! @@ optional anchor
//!  context / -removed / +added
//! *** End of File           (optional)
//! *** End Patch
//! ```
//!
//! Update hunks share the exact line syntax of [`super::patch`], so the hunk
//! applier is reused. All sections are planned before any change is returned.

use super::text::{self, preview};
use super::{EditError, FileChange, FileReader, patch};

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const END_OF_FILE: &str = "*** End of File";

pub(super) fn plan(text: &str, reader: &impl FileReader) -> Result<Vec<FileChange>, EditError> {
    let parse_err = |detail: String| EditError::Parse {
        mode: "apply-patch",
        detail,
    };
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

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            let path = path.trim().to_string();
            index += 1;
            let mut body = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                body.push(lines[index].strip_prefix('+').unwrap_or(lines[index]));
                index += 1;
            }
            if reader.read_text(&path).is_some() {
                return Err(EditError::FileExists(path));
            }
            let content = if body.is_empty() {
                String::new()
            } else {
                format!("{}\n", body.join("\n"))
            };
            changes.push(FileChange::Create { path, content });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            let path = path.trim().to_string();
            index += 1;
            if reader.read_text(&path).is_none() {
                return Err(EditError::MissingFile(path));
            }
            changes.push(FileChange::Delete { path });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let path = path.trim().to_string();
            index += 1;
            let mut move_to = None;
            if index < lines.len()
                && let Some(new_path) = lines[index].strip_prefix("*** Move to: ")
            {
                move_to = Some(new_path.trim().to_string());
                index += 1;
            }
            let mut hunk_lines = Vec::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                hunk_lines.push(lines[index]);
                index += 1;
            }
            if index < lines.len() && lines[index] == END_OF_FILE {
                index += 1;
            }
            let original = reader
                .read_text(&path)
                .ok_or_else(|| EditError::MissingFile(path.clone()))?;
            let newline = text::detect_newline(&original);
            let updated = patch::apply_hunks(&text::normalize(&original), &hunk_lines.join("\n"))?;
            let content = text::restore_newline(&updated, newline);
            match move_to {
                Some(to) => changes.push(FileChange::Rename {
                    from: path,
                    to,
                    content,
                }),
                None => changes.push(FileChange::Update { path, content }),
            }
            continue;
        }

        return Err(parse_err(format!(
            "unexpected line in patch: {}",
            preview(line)
        )));
    }

    if !closed {
        return Err(parse_err(format!("patch is missing `{END}`")));
    }
    if changes.is_empty() {
        return Err(EditError::Empty);
    }
    Ok(changes)
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

    #[test]
    fn add_update_move_and_delete() {
        let reader = files(&[
            ("src/app.py", "def greet():\n    print(\"Hi\")\n"),
            ("obsolete.txt", "stale\n"),
        ]);
        let patch = "*** Begin Patch\n\
*** Add File: hello.txt\n\
+Hello world\n\
*** Update File: src/app.py\n\
*** Move to: src/main.py\n\
@@ def greet():\n\
-    print(\"Hi\")\n\
+    print(\"Hello, world!\")\n\
*** Delete File: obsolete.txt\n\
*** End Patch\n";
        let changes = plan(patch, &reader).expect("apply-patch");
        assert_eq!(
            changes,
            vec![
                FileChange::Create {
                    path: "hello.txt".to_string(),
                    content: "Hello world\n".to_string(),
                },
                FileChange::Rename {
                    from: "src/app.py".to_string(),
                    to: "src/main.py".to_string(),
                    content: "def greet():\n    print(\"Hello, world!\")\n".to_string(),
                },
                FileChange::Delete {
                    path: "obsolete.txt".to_string(),
                },
            ]
        );
    }

    #[test]
    fn missing_envelope_errors() {
        let reader = files(&[]);
        let err = plan("*** Add File: x\n+y\n", &reader).expect_err("no begin");
        assert!(matches!(err, EditError::Parse { .. }));
    }

    #[test]
    fn unterminated_patch_errors() {
        let reader = files(&[]);
        let err = plan("*** Begin Patch\n*** Add File: x\n+y\n", &reader).expect_err("no end");
        assert!(matches!(err, EditError::Parse { .. }));
    }

    #[test]
    fn create_over_existing_errors() {
        let reader = files(&[("x", "old\n")]);
        let err = plan(
            "*** Begin Patch\n*** Add File: x\n+new\n*** End Patch\n",
            &reader,
        )
        .expect_err("exists");
        assert_eq!(err, EditError::FileExists("x".to_string()));
    }
}
