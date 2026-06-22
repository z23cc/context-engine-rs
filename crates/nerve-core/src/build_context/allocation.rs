use super::{
    BuildContextAllocationAttempt, BuildContextAllocationTrace, BuildContextExcludedFile,
    Candidate, SLICE_RADIUS, format_score, selection_key,
};
use crate::{
    CatalogEntry, CatalogProvider, LineRange, NerveError, ReadFileRequest, Selection,
    SelectionMode, WorkspaceContextInclude, WorkspaceContextRequest, count_tokens, read_file,
    workspace_context::{RenderCache, workspace_context_for_selection_cached},
};

pub(super) fn allocate_selection<P: CatalogProvider>(
    provider: &P,
    snapshot: &crate::CatalogSnapshot,
    ranked: &[Candidate<'_>],
    token_budget: usize,
    max_files: usize,
    render_cache: &mut RenderCache,
) -> Result<
    (
        Selection,
        Vec<BuildContextExcludedFile>,
        Vec<BuildContextAllocationTrace>,
    ),
    NerveError,
> {
    let mut selection = Selection::default();
    let mut excluded = Vec::new();
    let mut trace = Vec::new();

    for (idx, candidate) in ranked.iter().enumerate() {
        if selection.files.len() >= max_files {
            for candidate in &ranked[idx..] {
                excluded.push(excluded_file(candidate, "max_files"));
                trace.push(allocation_trace(
                    candidate,
                    Vec::new(),
                    "excluded",
                    "max_files",
                ));
            }
            break;
        }

        let mut included = false;
        let mut attempts = Vec::new();
        for mode in candidate_modes(provider, candidate, token_budget)? {
            let mut next_selection = selection.clone();
            next_selection
                .files
                .insert(selection_key(candidate.entry), mode.clone());
            let workspace = workspace_context_for_selection_cached(
                provider,
                snapshot,
                &next_selection,
                &WorkspaceContextRequest {
                    include: vec![
                        WorkspaceContextInclude::FileMap,
                        WorkspaceContextInclude::Contents,
                    ],
                    instructions: None,
                    ..Default::default()
                },
                render_cache,
            )?;
            let accepted = workspace.tokens.total_tokens <= token_budget;
            attempts.push(BuildContextAllocationAttempt {
                mode: allocation_mode_name(&mode).to_string(),
                total_tokens: workspace.tokens.total_tokens,
                accepted,
            });
            if accepted {
                selection = next_selection;
                included = true;
                break;
            }
        }

        if included {
            trace.push(allocation_trace(
                candidate, attempts, "included", "accepted",
            ));
        } else {
            excluded.push(excluded_file(candidate, "over_budget"));
            trace.push(allocation_trace(
                candidate,
                attempts,
                "excluded",
                "over_budget",
            ));
        }
    }

    Ok((selection, excluded, trace))
}

fn allocation_trace(
    candidate: &Candidate<'_>,
    attempts: Vec<BuildContextAllocationAttempt>,
    result: &str,
    reason: &str,
) -> BuildContextAllocationTrace {
    BuildContextAllocationTrace {
        path: candidate.entry.rel_path.clone(),
        display_path: candidate.display_path.clone(),
        score: format_score(candidate.score),
        score_breakdown: candidate.score_breakdown.clone(),
        attempts,
        result: result.to_string(),
        reason: reason.to_string(),
    }
}

fn excluded_file(candidate: &Candidate<'_>, reason: &str) -> BuildContextExcludedFile {
    BuildContextExcludedFile {
        path: candidate.entry.rel_path.clone(),
        display_path: candidate.display_path.clone(),
        score: format_score(candidate.score),
        score_breakdown: candidate.score_breakdown.clone(),
        reason: reason.to_string(),
    }
}

fn candidate_modes<P: CatalogProvider>(
    provider: &P,
    candidate: &Candidate<'_>,
    token_budget: usize,
) -> Result<Vec<SelectionMode>, NerveError> {
    let full_tokens = full_content_tokens(provider, candidate.entry)?;
    let ranges = hit_line_ranges(&candidate.hit_lines);
    let codemap_supported = provider
        .code_symbols_for_path(&candidate.entry.abs_path, &candidate.entry.rel_path)?
        .ok()
        .flatten()
        .is_some();

    let mut modes = vec![SelectionMode::Full];
    if codemap_supported {
        modes.push(SelectionMode::CodemapOnly);
    }
    let huge_with_hits = full_tokens > token_budget.saturating_div(2) && !ranges.is_empty();
    if (huge_with_hits || !codemap_supported) && !ranges.is_empty() {
        modes.push(SelectionMode::Slices(ranges));
    }
    Ok(modes)
}

fn full_content_tokens<P: CatalogProvider>(
    provider: &P,
    entry: &CatalogEntry,
) -> Result<usize, NerveError> {
    let response = read_file(
        provider,
        &ReadFileRequest {
            path: entry.abs_path.clone(),
            start_line: None,
            end_line: None,
            limit: None,
            snap: None,
        },
    )?;
    Ok(count_tokens(&response.content))
}

fn hit_line_ranges(lines: &std::collections::BTreeSet<usize>) -> Vec<LineRange> {
    lines
        .iter()
        .take(3)
        .map(|line| {
            LineRange::with_label(
                line.saturating_sub(SLICE_RADIUS).max(1),
                line.saturating_add(SLICE_RADIUS),
                format!("search hit near line {line}"),
            )
        })
        .collect()
}

fn allocation_mode_name(mode: &SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Full => "full",
        SelectionMode::Slices(_) => "slices",
        SelectionMode::CodemapOnly => "codemap_only",
    }
}
