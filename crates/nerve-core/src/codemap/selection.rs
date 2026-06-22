use super::*;

pub(super) fn select_entries<'a>(
    snapshot: &'a CatalogSnapshot,
    paths: &[PathBuf],
) -> Vec<&'a crate::models::CatalogEntry> {
    if paths.is_empty() {
        return snapshot.entries.iter().collect();
    }

    let mut selected = BTreeSet::new();
    for path in paths {
        let input = crate::path_match::PathMatchInput::from_path(path);
        for (idx, entry) in snapshot.entries.iter().enumerate() {
            if crate::path_match::entry_matches(snapshot, entry, &input) {
                selected.insert(idx);
            }
        }
    }

    selected
        .into_iter()
        .map(|idx| &snapshot.entries[idx])
        .collect()
}
