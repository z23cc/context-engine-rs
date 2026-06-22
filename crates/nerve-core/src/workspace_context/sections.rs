use crate::{
    codemap::{FileCodeStructure, render_file_codemap},
    models::{CatalogEntry, NerveError},
    port::CatalogProvider,
    selection::{Selection, SelectionKey},
    snapshot::CatalogSnapshot,
    token::count_tokens,
    tree::{FileTreeOptions, TreeMode, get_selected_file_tree_with_selection},
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedSection {
    pub(crate) text: String,
    pub(crate) token_count: usize,
}

pub(crate) fn render_selected_tree(
    snapshot: &CatalogSnapshot,
    selection: &Selection,
) -> RenderedSection {
    let response = get_selected_file_tree_with_selection(
        snapshot,
        &FileTreeOptions {
            mode: TreeMode::Auto,
            max_depth: None,
            path: None,
        },
        selection,
    );
    let mut body = String::new();
    if let Some(note) = response.note {
        body.push_str(&format!("note: {note}\n"));
    }
    if response.uses_legend {
        body.push_str("legend: * selected, + codemap-capable\n");
    }
    if !response.tree.is_empty() {
        body.push_str(response.tree.trim_end());
        body.push('\n');
    }
    if body.is_empty() {
        body.push_str("(no selected files)\n");
    }
    wrap_section("file_tree", &body)
}

pub(crate) fn render_selected_code<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &Selection,
) -> Result<RenderedSection, NerveError> {
    let entries = selected_entries(snapshot, selection);
    let mut body = String::new();
    let mut omitted = 0usize;

    for (_key, entry) in entries {
        match provider.code_symbols_for_path(&entry.abs_path, &entry.rel_path)? {
            Ok(Some(parsed)) => {
                let mut file = FileCodeStructure {
                    path: provider.display_path(&entry.abs_path),
                    language: parsed.language.clone(),
                    symbols: parsed.symbols.clone(),
                    token_count: 0,
                };
                file.token_count = count_tokens(&render_file_codemap(&file));
                body.push_str(&render_file_codemap(&file));
            }
            Ok(None) => omitted += 1,
            Err(message) => {
                let display_path = provider.display_path(&entry.abs_path);
                body.push_str(&format!("{display_path}\n  error: {message}\n"));
            }
        }
    }

    if body.is_empty() {
        body.push_str("(no symbols)\n");
    }
    if omitted > 0 {
        body.push_str(&format!(
            "({omitted} selected files omitted: unsupported or no symbols)\n"
        ));
    }
    Ok(wrap_section("code_structure", &body))
}

fn selected_entries<'a>(
    snapshot: &'a CatalogSnapshot,
    selection: &Selection,
) -> Vec<(SelectionKey, &'a CatalogEntry)> {
    let entries_by_key = snapshot
        .entries
        .iter()
        .map(|entry| (selection_key(entry), entry))
        .collect::<BTreeMap<_, _>>();

    selection
        .files
        .keys()
        .filter_map(|key| entries_by_key.get(key).map(|entry| (key.clone(), *entry)))
        .collect()
}

fn selection_key(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

fn wrap_section(name: &str, body: &str) -> RenderedSection {
    let text = format!("<{name}>\n{}</{name}>", body.trim_end());
    let token_count = count_tokens(&text);
    RenderedSection { text, token_count }
}
