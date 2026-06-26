//! `trace_path` — shortest call-chain from one symbol to another.
//!
//! Breadth-first search over outgoing callees: starting at `from`, expand each
//! symbol's callees (the same name-based, deterministic resolution
//! `call_hierarchy` uses for its outgoing direction) until `to` is reached or
//! `max_depth` is exhausted. Returns the shortest call chain `from → … → to`.
//!
//! It composes the existing `call_hierarchy` (outgoing) and `goto_definition`
//! rather than introducing a new graph, so it inherits their determinism and
//! their name-based, best-effort nature (not a scope/type resolver). The search
//! is deterministic — visited/parent state is ordered (`BTreeSet`/`BTreeMap`), the
//! queue is FIFO, and each node's callees come back in `call_hierarchy`'s stable
//! order — so the path is reproducible and golden-testable.

use super::*;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const TRACE_DEFAULT_MAX_DEPTH: usize = 8;
/// Callees fetched per expanded node.
const TRACE_OUTGOING_CAP: usize = 100;
/// Total symbols explored before the search gives up (runaway guard).
const TRACE_VISITED_CAP: usize = 2000;

const TRACE_NOTE: &str = "Shortest outgoing call chain from `from` to `to`, by breadth-first \
search over name-resolved callees (the same best-effort matching as call_hierarchy; not a \
scope/type resolver). Bounded by max_depth and an internal exploration cap.";

fn default_trace_depth() -> usize {
    TRACE_DEFAULT_MAX_DEPTH
}

/// Request for `trace_path`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TracePathRequest {
    /// Exact source symbol name (case-sensitive).
    pub from: String,
    /// Exact target symbol name (case-sensitive).
    pub to: String,
    /// Maximum call-chain length to search.
    #[serde(default = "default_trace_depth")]
    pub max_depth: usize,
    /// Optional display-language filter applied at every hop.
    #[serde(default)]
    pub language: Option<String>,
}

/// One node on a traced call chain (a symbol at its definition site).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathStep {
    pub symbol: String,
    pub display_path: String,
    pub line: usize,
}

/// Response for `trace_path`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracePathResponse {
    pub from: String,
    pub to: String,
    pub found: bool,
    pub max_depth: usize,
    /// The chain `from → … → to` when `found`; empty otherwise.
    pub path: Vec<PathStep>,
    pub note: String,
}

/// Find the shortest outgoing call chain from `from` to `to`.
pub fn trace_path<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &TracePathRequest,
) -> Result<TracePathResponse, NerveError> {
    trace_path_cancellable(
        provider,
        &owned_arc(snapshot),
        request,
        &CancelToken::never(),
    )
}

/// Cancellable [`trace_path`].
pub fn trace_path_cancellable<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    request: &TracePathRequest,
    cancel: &CancelToken,
) -> Result<TracePathResponse, NerveError> {
    let Some(from_step) = first_definition(
        provider,
        snapshot,
        &request.from,
        request.language.as_deref(),
        cancel,
    )?
    else {
        return Ok(empty_response(request));
    };
    if request.from == request.to {
        return Ok(found_response(request, vec![from_step]));
    }

    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(request.from.clone());
    // node name -> (parent name, this node's step), used to rebuild the chain.
    let mut parent: BTreeMap<String, (String, PathStep)> = BTreeMap::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((request.from.clone(), 0));

    while let Some((name, depth)) = queue.pop_front() {
        cancel.check_cancelled()?;
        if depth >= request.max_depth || visited.len() > TRACE_VISITED_CAP {
            continue;
        }
        let hierarchy = call_hierarchy_cancellable(
            provider,
            snapshot,
            &CallHierarchyRequest {
                symbol: name.clone(),
                direction: CallDirection::Outgoing,
                language: request.language.clone(),
                max_results: TRACE_OUTGOING_CAP,
            },
            cancel,
        )?;
        for edge in hierarchy.outgoing {
            let callee = edge.symbol.clone();
            if callee == request.to {
                parent.insert(callee.clone(), (name.clone(), step_of(&edge)));
                let path = reconstruct(&request.from, &request.to, &parent, from_step);
                return Ok(found_response(request, path));
            }
            if visited.insert(callee.clone()) {
                parent.insert(callee.clone(), (name.clone(), step_of(&edge)));
                queue.push_back((callee, depth + 1));
            }
        }
    }
    Ok(empty_response(request))
}

fn first_definition<P: CatalogProvider + Sync>(
    provider: &P,
    snapshot: &Arc<CatalogSnapshot>,
    name: &str,
    language: Option<&str>,
    cancel: &CancelToken,
) -> Result<Option<PathStep>, NerveError> {
    let request = NavigateRequest {
        symbol: name.to_string(),
        language: language.map(str::to_string),
        include_definitions: false,
        confident_only: false,
        max_results: 1,
    };
    let response = goto_definition_cancellable(provider, snapshot, &request, cancel)?;
    Ok(response
        .definitions
        .into_iter()
        .next()
        .map(|location| PathStep {
            symbol: name.to_string(),
            display_path: location.display_path,
            line: location.line,
        }))
}

fn step_of(edge: &CallEdge) -> PathStep {
    PathStep {
        symbol: edge.symbol.clone(),
        display_path: edge.display_path.clone(),
        line: edge.line,
    }
}

/// Climb `parent` from `to` back to `from`, then prepend `from`'s step, yielding
/// the chain in `from → … → to` order.
fn reconstruct(
    from: &str,
    to: &str,
    parent: &BTreeMap<String, (String, PathStep)>,
    from_step: PathStep,
) -> Vec<PathStep> {
    let mut steps = Vec::new();
    let mut current = to.to_string();
    while current != from {
        let Some((parent_name, step)) = parent.get(&current) else {
            break;
        };
        steps.push(step.clone());
        current = parent_name.clone();
    }
    steps.push(from_step);
    steps.reverse();
    steps
}

fn found_response(request: &TracePathRequest, path: Vec<PathStep>) -> TracePathResponse {
    TracePathResponse {
        from: request.from.clone(),
        to: request.to.clone(),
        found: true,
        max_depth: request.max_depth,
        path,
        note: TRACE_NOTE.to_string(),
    }
}

fn empty_response(request: &TracePathRequest) -> TracePathResponse {
    TracePathResponse {
        from: request.from.clone(),
        to: request.to.clone(),
        found: false,
        max_depth: request.max_depth,
        path: Vec::new(),
        note: TRACE_NOTE.to_string(),
    }
}
