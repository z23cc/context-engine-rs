use crate::codemap::CodeReference;
use std::{collections::BTreeSet, path::Path};

use super::{analysis::IndexedFile, language::language_family};

pub(crate) fn resolve_import_reference(
    files: &[IndexedFile],
    referencer_idx: usize,
    reference: &CodeReference,
) -> Option<usize> {
    let import_path = reference.import_path.as_deref()?;
    if reference.has_embedded_language() {
        return None;
    }
    let referencer = &files[referencer_idx];
    match language_family(reference.effective_language(&referencer.language)) {
        "rust" => resolve_rust_import(files, referencer, import_path),
        "python" => resolve_python_import(files, referencer, import_path),
        "javascript" => resolve_javascript_import(files, referencer, import_path),
        _ => None,
    }
}

fn resolve_rust_import(
    files: &[IndexedFile],
    referencer: &IndexedFile,
    import_path: &str,
) -> Option<usize> {
    let mut parts: Vec<_> = import_path
        .split("::")
        .filter(|part| !part.is_empty())
        .collect();
    while matches!(parts.first(), Some(&"crate" | &"self")) {
        parts.remove(0);
    }
    while matches!(parts.first(), Some(&"super")) {
        parts.remove(0);
    }
    if parts.is_empty() {
        return None;
    }

    let module_parts = &parts[..parts.len().saturating_sub(1)];
    let candidates = if module_parts.is_empty() {
        vec![format!("{}.rs", parts[0])]
    } else {
        let module = module_parts.join("/");
        vec![format!("{module}.rs"), format!("{module}/mod.rs")]
    };
    resolve_relative_candidates(files, referencer, &candidates)
}

fn resolve_python_import(
    files: &[IndexedFile],
    referencer: &IndexedFile,
    import_path: &str,
) -> Option<usize> {
    let parts: Vec<_> = import_path
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    let module = if parts.len() > 1 {
        &parts[..parts.len() - 1]
    } else {
        &parts[..]
    }
    .join("/");
    let candidates = vec![format!("{module}.py"), format!("{module}/__init__.py")];
    resolve_relative_candidates(files, referencer, &candidates)
}

fn resolve_javascript_import(
    files: &[IndexedFile],
    referencer: &IndexedFile,
    import_path: &str,
) -> Option<usize> {
    if import_path.starts_with('.') {
        let trimmed = import_path.trim_start_matches("./");
        let candidates = javascript_import_candidates(trimmed);
        return resolve_relative_candidates(files, referencer, &candidates);
    }
    None
}

fn javascript_import_candidates(path: &str) -> Vec<String> {
    let extensions = ["js", "jsx", "mjs", "cjs", "ts", "tsx"];
    if extensions
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
    {
        return vec![path.to_string()];
    }
    let mut candidates = Vec::new();
    for ext in extensions {
        candidates.push(format!("{path}.{ext}"));
    }
    for ext in extensions {
        candidates.push(format!("{path}/index.{ext}"));
    }
    candidates
}

fn resolve_relative_candidates(
    files: &[IndexedFile],
    referencer: &IndexedFile,
    candidates: &[String],
) -> Option<usize> {
    let base = Path::new(&referencer.path)
        .parent()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let mut normalized = BTreeSet::new();
    for candidate in candidates {
        normalized.insert(normalize_repo_path(candidate));
        if !base.is_empty() {
            normalized.insert(normalize_repo_path(&format!("{base}/{candidate}")));
        }
    }

    files.iter().enumerate().find_map(|(idx, file)| {
        normalized
            .contains(&normalize_repo_path(&file.path))
            .then_some(idx)
    })
}

fn normalize_repo_path(path: &str) -> String {
    let normalized_path = path.replace('\\', "/");
    let mut parts = Vec::new();
    for part in normalized_path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}
