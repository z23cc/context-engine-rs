use super::*;

/// Search an immutable snapshot using provider-backed content reads.
pub fn search_snapshot<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &SearchRequest,
) -> Result<SearchResponse, CtxError> {
    search_snapshot_cancellable(provider, snapshot, request, &CancelToken::never())
}

/// Search an immutable snapshot using provider-backed content reads, checking
/// `cancel` throughout the path/content hot loops.
pub fn search_snapshot_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &SearchRequest,
    cancel: &CancelToken,
) -> Result<SearchResponse, CtxError> {
    cancel.check_cancelled()?;
    let max_results = request.max_results.max(1);
    let case_sensitive = is_smart_case_sensitive(&request.pattern);
    let regex = build_search_regex(request, case_sensitive)?;
    let ac = build_literal_matcher(request, case_sensitive);
    let fuzzy_pattern = build_fuzzy_pattern(request, case_sensitive);
    let filter = EntryFilter::build(request)?;

    let (path_matches, path_total) = collect_path_matches(
        snapshot,
        &filter,
        PathQuery {
            pattern: &request.pattern,
            regex: regex.as_ref(),
            case_sensitive,
            whole_word: request.whole_word,
            fuzzy_pattern: fuzzy_pattern.as_ref(),
        },
        case_sensitive,
        max_results,
        cancel,
    )?;
    let content = collect_content_matches(
        ContentSearchInput {
            provider,
            snapshot,
            request,
            filter: &filter,
            regex: regex.as_ref(),
            ac: ac.as_ref(),
            case_sensitive,
            max_results,
        },
        cancel,
    )?;

    let total = path_total + content.total;
    let returned = path_matches.len() + content.matches.len();
    let omitted = total.saturating_sub(returned);
    Ok(SearchResponse {
        generation: snapshot.generation,
        path_matches,
        content_matches: content.matches,
        match_files: content.match_files,
        diagnostics: content.diagnostics,
        totals: SearchTotals {
            scanned_files: snapshot.entries.len(),
            path_matches: path_total,
            content_matches: content.total,
            omitted,
            content_files_scanned: content.files_scanned,
            content_bytes_scanned: content.bytes_scanned,
            binary_files_skipped: content.binary_files_skipped,
            content_file_limit: request.max_content_files,
            content_byte_limit: request.max_content_bytes,
            totals_are_lower_bound: content.exhausted,
            budget: SearchBudget {
                max_results,
                max_content_files: request.max_content_files,
                max_content_bytes: request.max_content_bytes,
                exhausted: omitted > 0 || content.exhausted,
            },
        },
    })
}
