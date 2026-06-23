use super::*;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Deserialize)]
pub(super) struct RenameSymbolArgs {
    pub(super) symbol: String,
    pub(super) new_name: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(default)]
    pub(super) kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RenameOccurrence {
    line: usize,
    column: usize,
}

struct RenameFilePlan {
    response_path: String,
    occurrences: BTreeSet<RenameOccurrence>,
}

struct RenameStale {
    display_path: String,
    line: usize,
    column: usize,
}

struct RenameScope {
    allowed_display_paths: BTreeSet<String>,
    aliased_importers: BTreeSet<String>,
    shadowed_importer: Option<String>,
}

enum RenameUpdateError {
    Dispatch(DispatchError),
    Stale(RenameStale),
}

impl From<DispatchError> for RenameUpdateError {
    fn from(value: DispatchError) -> Self {
        Self::Dispatch(value)
    }
}

pub(super) fn handle_rename_symbol<P>(
    provider: &P,
    arguments: Value,
    cancel: &CancelToken,
) -> Result<Value, DispatchError>
where
    P: DispatchProvider,
{
    let args: RenameSymbolArgs = serde_json::from_value(arguments)?;
    if !valid_symbol_name(&args.new_name) {
        return rename_symbol_noop(&args, "invalid_new_name", None, 0);
    }
    if args.symbol == args.new_name {
        return rename_symbol_noop(&args, "no_op", None, 0);
    }
    let snapshot = provider.snapshot_arc_cancellable(cancel)?;
    let found = read_symbol_for_rename(provider, &snapshot, &args, cancel)?;
    cancel.check_cancelled()?;
    if found.body.is_none() || found.total != 1 {
        return rename_symbol_noop(&args, "ambiguous_symbol", Some(found.matches), found.total);
    }
    let refs = references_for_rename(provider, &snapshot, &args, cancel)?;
    if refs.truncated {
        return rename_symbol_noop(
            &args,
            "too_many_references",
            Some(found.matches),
            refs.total,
        );
    }
    let definition = found.matches.first().expect("unique definition checked");
    let scope = rename_allowed_display_paths(provider, &snapshot, &args, definition, cancel)?;
    if scope.shadowed_importer.is_some() {
        return rename_symbol_noop(&args, "shadowed_importer", Some(found.matches), refs.total);
    }
    let references = rename_references_for_definition(&scope, &refs.references);
    let plans = rename_file_plans(&snapshot, &found, &references);
    let updates = match rename_updates(provider, plans, &args.symbol, &args.new_name) {
        Ok(updates) => updates,
        Err(RenameUpdateError::Stale(stale)) => return rename_symbol_stale(&args, stale),
        Err(RenameUpdateError::Dispatch(err)) => return Err(err),
    };
    tool_response_text(&apply_content_updates_at_paths_with_old(
        provider,
        "rename_symbol",
        updates,
        DiffOptions::default(),
    )?)
}

fn read_symbol_for_rename<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &std::sync::Arc<crate::CatalogSnapshot>,
    args: &RenameSymbolArgs,
    cancel: &CancelToken,
) -> Result<crate::navigate::ReadSymbolResponse, NerveError> {
    crate::navigate::read_symbol_cancellable(
        provider,
        snapshot,
        &crate::navigate::ReadSymbolRequest {
            symbol: args.symbol.clone(),
            path: args.path.clone(),
            language: args.language.clone(),
            kind: args.kind.clone(),
            include_body: true,
            max_matches: 20,
        },
        cancel,
    )
}

fn references_for_rename<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &std::sync::Arc<crate::CatalogSnapshot>,
    args: &RenameSymbolArgs,
    cancel: &CancelToken,
) -> Result<crate::navigate::ReferencesResponse, NerveError> {
    crate::navigate::find_references_cancellable(
        provider,
        snapshot,
        &crate::navigate::NavigateRequest {
            symbol: args.symbol.clone(),
            language: args.language.clone(),
            include_definitions: false,
            confident_only: true,
            max_results: usize::MAX,
        },
        cancel,
    )
}

fn rename_allowed_display_paths<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &std::sync::Arc<crate::CatalogSnapshot>,
    args: &RenameSymbolArgs,
    definition: &crate::navigate::SymbolLocation,
    cancel: &CancelToken,
) -> Result<RenameScope, NerveError> {
    let mut allowed = BTreeSet::from([definition.display_path.clone()]);
    let mut aliased_importers = BTreeSet::new();
    if snapshot.roots.len() > 1 {
        return Ok(RenameScope {
            allowed_display_paths: allowed,
            aliased_importers,
            shadowed_importer: None,
        });
    }
    let files = crate::graph::shared_indexed_files(provider, snapshot, cancel)?;
    let Some(definition_index) = files
        .iter()
        .position(|file| file.display_path == definition.display_path)
    else {
        return Ok(RenameScope {
            allowed_display_paths: allowed,
            aliased_importers,
            shadowed_importer: None,
        });
    };
    for (idx, file) in files.iter().enumerate() {
        let relationship =
            file_import_relationship(provider, &files, idx, definition_index, &args.symbol)?;
        if relationship.imports {
            if file_has_shadowing_signal(provider, file, &args.symbol)? {
                return Ok(RenameScope {
                    allowed_display_paths: allowed,
                    aliased_importers,
                    shadowed_importer: Some(file.display_path.clone()),
                });
            }
            if relationship.aliased {
                aliased_importers.insert(file.display_path.clone());
            }
            allowed.insert(file.display_path.clone());
        }
    }
    Ok(RenameScope {
        allowed_display_paths: allowed,
        aliased_importers,
        shadowed_importer: None,
    })
}

fn rename_references_for_definition(
    scope: &RenameScope,
    references: &[crate::navigate::ReferenceLocation],
) -> Vec<crate::navigate::ReferenceLocation> {
    references
        .iter()
        .filter(|reference| {
            scope
                .allowed_display_paths
                .contains(&reference.display_path)
        })
        .filter(|reference| {
            !scope.aliased_importers.contains(&reference.display_path) || reference.kind == "import"
        })
        .cloned()
        .collect()
}

fn rename_file_plans(
    snapshot: &crate::CatalogSnapshot,
    found: &crate::navigate::ReadSymbolResponse,
    references: &[crate::navigate::ReferenceLocation],
) -> BTreeMap<String, RenameFilePlan> {
    let mut plans = BTreeMap::new();
    if let Some(definition) = found.matches.first() {
        add_rename_occurrence(snapshot, &mut plans, definition);
    }
    for reference in references {
        add_reference_occurrence(snapshot, &mut plans, reference);
    }
    plans
}

fn add_rename_occurrence(
    snapshot: &crate::CatalogSnapshot,
    plans: &mut BTreeMap<String, RenameFilePlan>,
    location: &crate::navigate::SymbolLocation,
) {
    add_occurrence(
        plans,
        edit_path_for_location(snapshot, &location.path, &location.display_path),
        location.display_path.clone(),
        location.line,
        location.column,
    );
}

fn add_reference_occurrence(
    snapshot: &crate::CatalogSnapshot,
    plans: &mut BTreeMap<String, RenameFilePlan>,
    location: &crate::navigate::ReferenceLocation,
) {
    add_occurrence(
        plans,
        edit_path_for_location(snapshot, &location.path, &location.display_path),
        location.display_path.clone(),
        location.line,
        location.column,
    );
}

fn add_occurrence(
    plans: &mut BTreeMap<String, RenameFilePlan>,
    edit_path: String,
    response_path: String,
    line: usize,
    column: usize,
) {
    plans
        .entry(edit_path)
        .or_insert_with(|| RenameFilePlan {
            response_path,
            occurrences: BTreeSet::new(),
        })
        .occurrences
        .insert(RenameOccurrence { line, column });
}

fn edit_path_for_location(
    snapshot: &crate::CatalogSnapshot,
    path: &str,
    display_path: &str,
) -> String {
    snapshot
        .entries
        .iter()
        .find(|entry| {
            entry.rel_path == path
                && snapshot_display_path(snapshot, &entry.root_id, &entry.rel_path) == display_path
        })
        .map(|entry| entry.abs_path.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn snapshot_display_path(
    snapshot: &crate::CatalogSnapshot,
    root_id: &str,
    rel_path: &str,
) -> String {
    if snapshot.roots.len() <= 1 {
        return rel_path.to_string();
    }
    format!("{root_id}/{rel_path}")
}

fn rename_updates<P: CatalogProvider + ?Sized>(
    provider: &P,
    plans: BTreeMap<String, RenameFilePlan>,
    old_name: &str,
    new_name: &str,
) -> Result<Vec<ContentUpdate>, RenameUpdateError> {
    let mut updates = Vec::new();
    for (edit_path, plan) in plans {
        let old = read_current(provider, &edit_path)?;
        let content =
            rename_content(&old, &plan.occurrences, old_name, new_name).map_err(|stale| {
                RenameUpdateError::Stale(RenameStale {
                    display_path: plan.response_path.clone(),
                    line: stale.line,
                    column: stale.column,
                })
            })?;
        updates.push(ContentUpdate {
            edit_path,
            response_path: plan.response_path,
            content,
            old,
        });
    }
    Ok(updates)
}

fn rename_content(
    source: &str,
    occurrences: &BTreeSet<RenameOccurrence>,
    old_name: &str,
    new_name: &str,
) -> Result<String, RenameOccurrence> {
    let mut lines: Vec<String> = split_lines_preserve(source)
        .into_iter()
        .map(str::to_string)
        .collect();
    let mut ordered: Vec<_> = occurrences.iter().collect();
    ordered.sort_by(|a, b| b.line.cmp(&a.line).then(b.column.cmp(&a.column)));
    for occurrence in ordered {
        if !replace_occurrence(&mut lines, occurrence, old_name, new_name) {
            return Err(occurrence.clone());
        }
    }
    Ok(lines.concat())
}

fn replace_occurrence(
    lines: &mut [String],
    occurrence: &RenameOccurrence,
    old_name: &str,
    new_name: &str,
) -> bool {
    let Some(line) = lines.get_mut(occurrence.line.saturating_sub(1)) else {
        return false;
    };
    let start = occurrence.column.saturating_sub(1);
    let end = start.saturating_add(old_name.len());
    if line.get(start..end) != Some(old_name) {
        return false;
    }
    line.replace_range(start..end, new_name);
    true
}

fn valid_symbol_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !reserved_symbol_name(name)
}

fn reserved_symbol_name(name: &str) -> bool {
    matches!(
        name,
        "as" | "async"
            | "await"
            | "break"
            | "class"
            | "const"
            | "continue"
            | "def"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "fn"
            | "for"
            | "from"
            | "function"
            | "if"
            | "impl"
            | "import"
            | "in"
            | "interface"
            | "let"
            | "match"
            | "mod"
            | "mut"
            | "null"
            | "pub"
            | "return"
            | "self"
            | "static"
            | "struct"
            | "super"
            | "this"
            | "trait"
            | "true"
            | "type"
            | "use"
            | "var"
            | "while"
            | "yield"
    )
}

fn rename_symbol_noop(
    args: &RenameSymbolArgs,
    reason: &str,
    matches: Option<Vec<crate::navigate::SymbolLocation>>,
    total: usize,
) -> Result<Value, DispatchError> {
    let text = format!(
        "rename_symbol: {} -> {} not applied ({reason})\n",
        args.symbol, args.new_name
    );
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": {
            "mutated": false,
            "symbol": args.symbol,
            "new_name": args.new_name,
            "reason": reason,
            "matches": matches.unwrap_or_default(),
            "total": total,
        },
    }))
}

fn rename_symbol_stale(
    args: &RenameSymbolArgs,
    stale: RenameStale,
) -> Result<Value, DispatchError> {
    let text = format!(
        "rename_symbol: {} -> {} not applied (stale_occurrence at {}:{}:{})\n",
        args.symbol, args.new_name, stale.display_path, stale.line, stale.column
    );
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": {
            "mutated": false,
            "symbol": args.symbol,
            "new_name": args.new_name,
            "reason": "stale_occurrence",
            "path": stale.display_path,
            "line": stale.line,
            "column": stale.column,
            "reread_hint": "Call read_symbol/find_references again, then retry rename_symbol."
        },
    }))
}

fn read_current<P: CatalogProvider + ?Sized>(
    provider: &P,
    path: &str,
) -> Result<String, DispatchError> {
    Ok(String::from_utf8_lossy(&provider.read_bytes(Path::new(path))?).into_owned())
}

fn split_lines_preserve(source: &str) -> Vec<&str> {
    source.split_inclusive('\n').collect()
}
