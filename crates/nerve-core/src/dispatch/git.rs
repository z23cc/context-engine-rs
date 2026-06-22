use super::{DispatchError, GitArgs, edit};
use serde_json::{Value, json};
use std::{cmp::Reverse, path::Path};

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffFile {
    path: String,
    insertions: Option<usize>,
    deletions: Option<usize>,
}

impl DiffFile {
    fn churn(&self) -> usize {
        self.insertions.unwrap_or(0) + self.deletions.unwrap_or(0)
    }

    fn label(&self) -> String {
        format!(
            "{} (+{} -{})",
            self.path,
            format_count(self.insertions),
            format_count(self.deletions)
        )
    }
}

struct DiffPatch {
    file: DiffFile,
    patch: String,
    truncated: bool,
}

pub(super) struct GitRunOutput {
    pub(super) text: String,
    pub(super) structured: Value,
}

pub(super) fn run_git_response(root: &Path, args: &GitArgs) -> Result<GitRunOutput, DispatchError> {
    if args.op == "diff" && args.detail.as_deref() == Some("bundle") {
        validate_path(args.path.as_deref())?;
        validate_max_chars(args.max_chars)?;
        return run_diff_bundle(root, args);
    }
    let text = run_git(root, args)?;
    Ok(GitRunOutput {
        structured: json!({ "op": args.op, "output": text }),
        text,
    })
}

/// Run a read-only git subcommand in `root` and return its stdout (capped).
pub(super) fn run_git(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    validate_path(args.path.as_deref())?;
    validate_max_chars(args.max_chars)?;
    match args.op.as_str() {
        "status" => run_capped(root, &["status", "--short", "--branch"], args.max_chars),
        "diff" => run_diff(root, args),
        "log" => run_log(root, args),
        "blame" => run_blame(root, args),
        "show" => run_show(root, args),
        other => Err(git_error(format!("unknown git op: {other}"))),
    }
}

fn run_diff(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    match args.detail.as_deref().unwrap_or("full") {
        "summary" => run_diff_summary(root, args),
        "files" => run_diff_files(root, args),
        "patches" => run_churn_sorted_patches(root, args),
        "full" => run_capped(root, &diff_command(args, None), args.max_chars),
        other => Err(git_error(format!("unknown git diff detail: {other}"))),
    }
}

fn run_log(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let mut git = vec!["log".to_string(), "--oneline".to_string(), "-n".to_string()];
    git.push(args.count.to_string());
    add_path_scope(&mut git, args.path.as_deref());
    run_capped(root, &git, args.max_chars)
}

fn run_blame(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let path = args
        .path
        .as_ref()
        .ok_or_else(|| git_error("blame requires a path"))?;
    let mut git = vec!["blame".to_string()];
    if let Some(lines) = &args.lines {
        git.push("-L".to_string());
        git.push(lines.clone());
    }
    add_path_scope(&mut git, Some(path));
    run_capped(root, &git, args.max_chars)
}

fn run_show(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let reference = args
        .reference
        .as_ref()
        .ok_or_else(|| git_error("show requires a ref"))?;
    run_capped(
        root,
        &["show".to_string(), reference.clone()],
        args.max_chars,
    )
}

fn run_diff_summary(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let text = run_git_command(root, &diff_command_with_flag(args, "--shortstat"))?;
    Ok(non_empty_or_no_changes(text))
}

fn run_diff_files(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let files = changed_files(root, args)?;
    if files.is_empty() {
        return Ok("(no changes)\n".to_string());
    }
    let mut out = String::from("changed files (churn-sorted):\n");
    for file in files {
        out.push_str(&format!("  {}\n", file.label()));
    }
    Ok(cap_text(out, args.max_chars, "output truncated"))
}

fn run_churn_sorted_patches(root: &Path, args: &GitArgs) -> Result<String, DispatchError> {
    let files = changed_files(root, args)?;
    if files.is_empty() {
        return Ok("(no changes)\n".to_string());
    }
    let mut out = format!(
        "git diff patches (churn-sorted, max_chars={}):\n",
        args.max_chars
    );
    let mut included = 0usize;
    for file in &files {
        let segment = patch_segment(root, args, file)?;
        if !append_segment(&mut out, &segment, args.max_chars) {
            break;
        }
        included += 1;
    }
    if included < files.len() {
        out.push_str(&format!(
            "\n\u{2026} (diff truncated; omitted {} changed file(s); increase max_chars or pass path)\n",
            files.len() - included
        ));
    }
    Ok(out)
}

fn run_diff_bundle(root: &Path, args: &GitArgs) -> Result<GitRunOutput, DispatchError> {
    let files = changed_files(root, args)?;
    let summary = run_diff_summary(root, args)?;
    let (patches, truncated) = collect_bundle_patches(root, args, &files)?;
    let text = cap_text(
        render_diff_bundle_text(args, &summary, &files, &patches, truncated),
        args.max_chars,
        "output truncated",
    );
    let structured = diff_bundle_structured(args, &summary, &files, &patches, truncated);
    Ok(GitRunOutput { text, structured })
}

fn collect_bundle_patches(
    root: &Path,
    args: &GitArgs,
    files: &[DiffFile],
) -> Result<(Vec<DiffPatch>, bool), DispatchError> {
    let mut patches = Vec::new();
    let mut used = 0usize;
    for file in files {
        let patch = run_git_command(root, &diff_command(args, Some(&file.path)))?;
        let remaining = args.max_chars.saturating_sub(used);
        let patch_chars = patch.chars().count();
        if patch_chars <= remaining {
            used += patch_chars;
            patches.push(DiffPatch {
                file: file.clone(),
                patch,
                truncated: false,
            });
            continue;
        }
        if remaining > 0 {
            patches.push(DiffPatch {
                file: file.clone(),
                patch: take_chars(&patch, remaining),
                truncated: true,
            });
        }
        return Ok((patches, true));
    }
    Ok((patches, false))
}

fn render_diff_bundle_text(
    args: &GitArgs,
    summary: &str,
    files: &[DiffFile],
    patches: &[DiffPatch],
    truncated: bool,
) -> String {
    if files.is_empty() {
        return "git diff bundle:\n(no changes)\n".to_string();
    }
    let mut out = format!(
        "git diff bundle (churn-sorted, patch_budget={}):\n",
        args.max_chars
    );
    out.push_str(summary.trim_end());
    out.push_str("\nfiles:\n");
    for file in files {
        out.push_str(&format!("  {}\n", file.label()));
    }
    out.push_str(&format!(
        "patches: included {}/{}",
        patches.len(),
        files.len()
    ));
    if truncated {
        out.push_str(" (truncated; increase max_chars or pass path)");
    }
    out.push('\n');
    out
}

fn diff_bundle_structured(
    args: &GitArgs,
    summary: &str,
    files: &[DiffFile],
    patches: &[DiffPatch],
    truncated: bool,
) -> Value {
    let truncated_patch_count = patches.iter().filter(|patch| patch.truncated).count();
    json!({
        "op": "diff",
        "detail": "bundle",
        "staged": args.staged,
        "path": args.path,
        "max_chars": args.max_chars,
        "summary": summary,
        "files": files.iter().map(diff_file_json).collect::<Vec<_>>(),
        "patches": patches.iter().map(diff_patch_json).collect::<Vec<_>>(),
        "included_patch_count": patches.len(),
        "omitted_patch_count": files.len().saturating_sub(patches.len()),
        "truncated_patch_count": truncated_patch_count,
        "truncated": truncated,
        "truncation": bundle_truncation(files, patches, truncated),
    })
}

fn bundle_truncation(files: &[DiffFile], patches: &[DiffPatch], truncated: bool) -> Value {
    if !truncated {
        return Value::Null;
    }
    json!({
        "reason": "patch payload exceeded max_chars",
        "omitted_patch_count": files.len().saturating_sub(patches.len()),
        "truncated_patch_count": patches.iter().filter(|patch| patch.truncated).count(),
    })
}

fn diff_file_json(file: &DiffFile) -> Value {
    json!({
        "path": file.path,
        "insertions": file.insertions,
        "deletions": file.deletions,
        "churn": file.churn(),
        "binary": file.insertions.is_none() || file.deletions.is_none(),
    })
}

fn diff_patch_json(patch: &DiffPatch) -> Value {
    json!({
        "path": patch.file.path,
        "insertions": patch.file.insertions,
        "deletions": patch.file.deletions,
        "churn": patch.file.churn(),
        "patch": patch.patch,
        "truncated": patch.truncated,
    })
}

fn patch_segment(root: &Path, args: &GitArgs, file: &DiffFile) -> Result<String, DispatchError> {
    let patch = run_git_command(root, &diff_command(args, Some(&file.path)))?;
    Ok(format!("\n# file: {}\n{patch}", file.label()))
}

fn changed_files(root: &Path, args: &GitArgs) -> Result<Vec<DiffFile>, DispatchError> {
    let text = run_git_command(root, &diff_command_with_flags(args, &["--numstat", "-z"]))?;
    let mut files = parse_numstat_z(&text);
    files.sort_by_key(|file| (Reverse(file.churn()), file.path.clone()));
    Ok(files)
}

fn parse_numstat_z(text: &str) -> Vec<DiffFile> {
    text.split('\0').filter_map(parse_numstat_line).collect()
}

fn parse_numstat_line(line: &str) -> Option<DiffFile> {
    let mut parts = line.splitn(3, '\t');
    let insertions = parse_count(parts.next()?);
    let deletions = parse_count(parts.next()?);
    let path = parts.next()?.to_string();
    Some(DiffFile {
        path,
        insertions,
        deletions,
    })
}

fn parse_count(value: &str) -> Option<usize> {
    (value != "-").then(|| value.parse().ok()).flatten()
}

fn format_count(count: Option<usize>) -> String {
    count.map_or_else(|| "-".to_string(), |n| n.to_string())
}

fn diff_command_with_flag(args: &GitArgs, flag: &str) -> Vec<String> {
    diff_command_with_flags(args, &[flag])
}

fn diff_command_with_flags(args: &GitArgs, flags: &[&str]) -> Vec<String> {
    let mut git = diff_command(args, None);
    for flag in flags.iter().rev() {
        git.insert(2, (*flag).to_string());
    }
    git
}

fn diff_command(args: &GitArgs, path_override: Option<&str>) -> Vec<String> {
    let mut git = vec!["diff".to_string(), "--no-ext-diff".to_string()];
    if args.staged {
        git.push("--staged".to_string());
    }
    add_path_scope(&mut git, path_override.or(args.path.as_deref()));
    git
}

fn add_path_scope(git: &mut Vec<String>, path: Option<&str>) {
    if let Some(path) = path {
        git.push("--".to_string());
        git.push(literal_pathspec(path));
    }
}

fn literal_pathspec(path: &str) -> String {
    format!(":(literal){path}")
}

fn append_segment(out: &mut String, segment: &str, max_chars: usize) -> bool {
    let used = out.chars().count();
    if used + segment.chars().count() <= max_chars {
        out.push_str(segment);
        return true;
    }
    let remaining = max_chars.saturating_sub(used);
    if remaining > 0 {
        out.push_str(&take_chars(segment, remaining));
    }
    false
}

fn run_capped(
    root: &Path,
    git: &[impl AsRef<str>],
    max_chars: usize,
) -> Result<String, DispatchError> {
    let text = run_git_command(root, git)?;
    Ok(cap_text(text, max_chars, "output truncated"))
}

fn run_git_command(root: &Path, git: &[impl AsRef<str>]) -> Result<String, DispatchError> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(git.iter().map(AsRef::as_ref))
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|err| git_error(format!("could not run git: {err}")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(git_error(format!(
        "git {} failed: {}",
        git.first().map(AsRef::as_ref).unwrap_or("command"),
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn cap_text(text: String, max_chars: usize, label: &str) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    format!("{}\n\u{2026} ({label})\n", take_chars(&text, max_chars))
}

fn take_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn non_empty_or_no_changes(text: String) -> String {
    if text.trim().is_empty() {
        "(no changes)\n".to_string()
    } else {
        text
    }
}

fn validate_path(path: Option<&str>) -> Result<(), DispatchError> {
    if path.is_some_and(has_parent_segment) {
        return Err(git_error("path traversal is not allowed"));
    }
    Ok(())
}

fn validate_max_chars(max_chars: usize) -> Result<(), DispatchError> {
    if max_chars == 0 {
        return Err(git_error("max_chars must be greater than 0"));
    }
    Ok(())
}

fn has_parent_segment(path: &str) -> bool {
    path.split(['/', '\\']).any(|segment| segment == "..")
}

fn git_error(detail: impl Into<String>) -> DispatchError {
    DispatchError::Edit(edit::EditError::Parse {
        mode: "git",
        detail: detail.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_parsing_and_churn_label() {
        let file = parse_numstat_line("12\t3\tsrc/lib.rs").expect("parsed");
        assert_eq!(file.churn(), 15);
        assert_eq!(file.label(), "src/lib.rs (+12 -3)");
        let with_tab = parse_numstat_line("2\t1\tsrc/a\tb.rs").expect("parsed");
        assert_eq!(with_tab.path, "src/a\tb.rs");
    }

    #[test]
    fn numstat_z_parsing_keeps_spaces() {
        let files = parse_numstat_z("4\t4\tb file.txt\0");
        assert_eq!(files[0].label(), "b file.txt (+4 -4)");
    }

    #[test]
    fn binary_numstat_counts_as_zero_churn() {
        let file = parse_numstat_line("-\t-\timage.png").expect("parsed");
        assert_eq!(file.churn(), 0);
        assert_eq!(file.label(), "image.png (+- --)");
    }

    #[test]
    fn append_segment_caps_on_char_boundary() {
        let mut out = String::from("abc");
        assert!(!append_segment(&mut out, "déf", 5));
        assert_eq!(out, "abcdé");
    }
}
