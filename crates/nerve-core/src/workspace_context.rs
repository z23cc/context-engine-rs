//! Workspace-context snapshot assembly from the persistent selection.

mod files;
mod label;
mod sections;

use crate::{
    models::{CatalogEntry, NerveError},
    port::CatalogProvider,
    recipe::MetaPrompt,
    selection::{Selection, SelectionKey, SelectionMode},
    snapshot::CatalogSnapshot,
    token::count_tokens,
};
use files::render_selected_file;
use sections::{render_selected_code, render_selected_tree};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Optional text sections to include in the assembled context text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceContextInclude {
    #[serde(rename = "file-map")]
    FileMap,
    Contents,
    Tokens,
    Tree,
    Code,
    #[serde(rename = "git-diff")]
    GitDiff,
    #[serde(rename = "meta-prompts")]
    MetaPrompts,
}

/// Request for the `workspace_context` snapshot tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WorkspaceContextRequest {
    #[serde(default)]
    pub include: Vec<WorkspaceContextInclude>,
    pub instructions: Option<String>,
    /// A named recipe (standard|plan|review|diff|manual). When set (and not
    /// `manual`) it fixes the section set, overriding `include`.
    #[serde(default)]
    pub recipe: Option<String>,
    /// Working-tree diff text to render in the `<git_diff>` section. The caller
    /// supplies it (e.g. from the `git` tool); the kernel never runs git.
    #[serde(default)]
    pub git_diff: Option<String>,
    /// Reusable instruction blocks, rendered as numbered `<meta prompt>` sections.
    /// When empty, a recipe's default meta-prompts (Plan/Review) are used.
    #[serde(default)]
    pub meta_prompts: Vec<MetaPrompt>,
}

/// Structured response for `workspace_context`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceContextResponse {
    /// Assembled context text. Not serialized into `structuredContent`: it is
    /// already the tool's `content[].text`, so emitting it twice would double
    /// the payload. The token breakdown stays structured.
    #[serde(default, skip_serializing)]
    pub context: String,
    /// Deterministic hash of the exact rendered `content[].text` payload.
    pub context_hash: String,
    pub tokens: WorkspaceContextTokenBreakdown,
}

/// Token breakdown for the assembled workspace context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceContextTokenBreakdown {
    /// Total tokens for the assembled payload excluding the optional token report.
    pub total_tokens: usize,
    pub file_map_tokens: usize,
    pub instructions_tokens: usize,
    pub contents_tokens: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tree_tokens: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub code_tokens: usize,
    pub git_diff_tokens: usize,
    pub meta_prompts_tokens: usize,
    pub files: Vec<WorkspaceContextFileTokens>,
}

/// Per-file token breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceContextFileTokens {
    pub root_id: String,
    pub path: String,
    pub display_path: String,
    pub mode: String,
    /// Deterministic hash of this file's rendered context block.
    pub content_hash: String,
    pub token_count: usize,
    pub segments: Vec<WorkspaceContextSegmentTokens>,
}

/// Per-segment token breakdown for slices and file sections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceContextSegmentTokens {
    pub label: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub token_count: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// Assemble the current provider selection into a context snapshot.
pub fn workspace_context<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    request: &WorkspaceContextRequest,
) -> Result<WorkspaceContextResponse, NerveError> {
    let selection = provider.selection();
    workspace_context_for_selection(provider, snapshot, &selection, request)
}

/// Assemble an explicit selection without mutating provider-owned state.
pub fn workspace_context_for_selection<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &Selection,
    request: &WorkspaceContextRequest,
) -> Result<WorkspaceContextResponse, NerveError> {
    let mut cache = RenderCache::new(snapshot.generation);
    workspace_context_for_selection_cached(provider, snapshot, selection, request, &mut cache)
}

/// Per-file render memoization for repeated `workspace_context` assembly.
///
/// `build_context`'s greedy allocator assembles many overlapping selections
/// that differ by a single trial file. Rendering each already-selected file
/// from scratch on every trial is `O(files²)`. Rendered text and its token
/// breakdown are a pure function of `(SelectionKey, SelectionMode, generation)`,
/// so they are cached here and reused byte-for-byte across trials. The final
/// `count_tokens` over the assembled context is *not* cached: BPE is not
/// additive across the `\n\n` joins, so the whole-string count is the only
/// faithful total and must be recomputed per assembly.
pub(crate) struct RenderCache {
    generation: u64,
    entries: BTreeMap<(SelectionKey, SelectionMode), RenderedFile>,
}

impl RenderCache {
    pub(crate) fn new(generation: u64) -> Self {
        Self {
            generation,
            entries: BTreeMap::new(),
        }
    }
}

/// Assemble an explicit selection, memoizing per-file rendering across calls.
pub(crate) fn workspace_context_for_selection_cached<P: CatalogProvider>(
    provider: &P,
    snapshot: &CatalogSnapshot,
    selection: &Selection,
    request: &WorkspaceContextRequest,
    cache: &mut RenderCache,
) -> Result<WorkspaceContextResponse, NerveError> {
    debug_assert_eq!(
        cache.generation, snapshot.generation,
        "RenderCache reused across snapshot generations"
    );
    let include = IncludeSet::from_request(request);
    let selected = selected_entries(provider, snapshot, selection);

    let file_map = render_file_map(&selected);
    let file_map_tokens = if include.file_map {
        count_tokens(&file_map)
    } else {
        0
    };

    let file_tree = include
        .tree
        .then(|| render_selected_tree(snapshot, selection));
    let tree_tokens = file_tree.as_ref().map_or(0, |section| section.token_count);

    let code_structure = if include.code {
        Some(render_selected_code(provider, snapshot, selection)?)
    } else {
        None
    };
    let code_tokens = code_structure
        .as_ref()
        .map_or(0, |section| section.token_count);

    let mut file_tokens = Vec::new();
    let mut rendered_files = Vec::new();
    for selected_file in selected {
        let rendered = render_selected_file_cached(provider, &selected_file, cache)?;
        file_tokens.push(rendered.tokens);
        rendered_files.push(rendered.text);
    }

    let contents_text = rendered_files.join("\n\n");
    let contents_tokens = if include.contents {
        file_tokens.iter().map(|file| file.token_count).sum()
    } else {
        0
    };

    // git_diff section: caller-supplied text only — the kernel never runs git.
    let git_diff = include
        .git_diff
        .then_some(request.git_diff.as_deref())
        .flatten()
        .filter(|diff| !diff.trim().is_empty())
        .map(render_git_diff);
    let git_diff_tokens = git_diff.as_deref().map_or(0, count_tokens);

    // meta-prompts: caller-supplied, else the named recipe's defaults.
    let meta_prompts = if include.meta_prompts {
        resolve_meta_prompts(request)
    } else {
        Vec::new()
    };
    let meta_prompts_text = (!meta_prompts.is_empty()).then(|| render_meta_prompts(&meta_prompts));
    let meta_prompts_tokens = meta_prompts_text.as_deref().map_or(0, count_tokens);

    let instructions = request.instructions.as_deref().map(render_instructions);
    let instructions_tokens = instructions.as_deref().map_or(0, count_tokens);

    // Ordered sections: file_map, file_tree, code_structure, file_contents,
    // git_diff, meta_prompts, instructions (instructions last).
    let mut sections = Vec::new();
    if include.file_map {
        sections.push(file_map);
    }
    if let Some(file_tree) = file_tree {
        sections.push(file_tree.text);
    }
    if let Some(code_structure) = code_structure {
        sections.push(code_structure.text);
    }
    if include.contents && !contents_text.is_empty() {
        sections.push(contents_text);
    }
    if let Some(git_diff) = git_diff {
        sections.push(git_diff);
    }
    if let Some(meta_prompts_text) = meta_prompts_text {
        sections.push(meta_prompts_text);
    }
    if let Some(instructions) = instructions {
        sections.push(instructions);
    }

    let context = sections.join("\n\n");
    let total_tokens = count_tokens(&context);
    let mut response = WorkspaceContextResponse {
        context,
        context_hash: String::new(),
        tokens: WorkspaceContextTokenBreakdown {
            total_tokens,
            file_map_tokens,
            instructions_tokens,
            contents_tokens,
            tree_tokens,
            code_tokens,
            git_diff_tokens,
            meta_prompts_tokens,
            files: file_tokens,
        },
    };

    if include.tokens {
        append_token_report(&mut response);
    }

    response.context_hash = content_hash(&response.context);
    Ok(response)
}

fn append_token_report(response: &mut WorkspaceContextResponse) {
    let token_report =
        serde_json::to_string_pretty(&response.tokens).expect("token breakdown serializes");
    if response.context.is_empty() {
        response.context = format!("<tokens>\n{token_report}\n</tokens>");
    } else {
        response
            .context
            .push_str(&format!("\n\n<tokens>\n{token_report}\n</tokens>"));
    }
}

fn content_hash(text: &str) -> String {
    let mut low = 0xcbf2_9ce4_8422_2325u64;
    let mut high = 0x9e37_79b9_7f4a_7c15u64;
    for byte in text.as_bytes() {
        low ^= u64::from(*byte);
        low = low.wrapping_mul(0x0000_0100_0000_01b3);
        high ^= u64::from(byte.rotate_left(1));
        high = high.wrapping_mul(0x0000_0001_0000_01b3);
    }
    let hash = (u128::from(high) << 64) | u128::from(low);
    format!("{hash:032x}")
}

#[derive(Debug)]
struct IncludeSet {
    file_map: bool,
    contents: bool,
    tokens: bool,
    git_diff: bool,
    meta_prompts: bool,
    tree: bool,
    code: bool,
}

impl IncludeSet {
    fn from_request(request: &WorkspaceContextRequest) -> Self {
        use WorkspaceContextInclude::{
            Code, Contents, FileMap, GitDiff, MetaPrompts, Tokens, Tree,
        };
        // A named recipe (except `manual`) fixes the section set, overriding `include`.
        if let Some(recipe) = request
            .recipe
            .as_deref()
            .filter(|name| *name != "manual")
            .and_then(crate::recipe::recipe_by_name)
        {
            let has = |section| recipe.sections.contains(&section);
            return Self {
                file_map: has(FileMap),
                contents: has(Contents),
                git_diff: has(GitDiff),
                meta_prompts: has(MetaPrompts),
                tree: has(Tree),
                code: has(Code),
                tokens: has(Tokens),
            };
        }
        if request.include.is_empty() {
            return Self {
                file_map: true,
                contents: true,
                tokens: false,
                git_diff: false,
                meta_prompts: false,
                tree: false,
                code: false,
            };
        }
        Self {
            file_map: request.include.contains(&FileMap),
            contents: request.include.contains(&Contents),
            tokens: request.include.contains(&Tokens),
            git_diff: request.include.contains(&GitDiff),
            meta_prompts: request.include.contains(&MetaPrompts),
            tree: request.include.contains(&Tree),
            code: request.include.contains(&Code),
        }
    }
}

#[derive(Debug)]
struct SelectedFile<'a> {
    key: SelectionKey,
    entry: &'a CatalogEntry,
    display_path: String,
    mode: SelectionMode,
}

#[derive(Debug, Clone)]
struct RenderedFile {
    text: String,
    tokens: WorkspaceContextFileTokens,
}

fn selected_entries<'a, P: CatalogProvider>(
    provider: &P,
    snapshot: &'a CatalogSnapshot,
    selection: &Selection,
) -> Vec<SelectedFile<'a>> {
    let entries_by_key = snapshot
        .entries
        .iter()
        .map(|entry| (selection_key(entry), entry))
        .collect::<BTreeMap<_, _>>();

    selection
        .files
        .clone()
        .into_iter()
        .filter_map(|(key, mode)| {
            entries_by_key.get(&key).map(|entry| SelectedFile {
                key,
                entry,
                display_path: provider.display_path(&entry.abs_path),
                mode,
            })
        })
        .collect()
}

fn render_file_map(files: &[SelectedFile<'_>]) -> String {
    let mut lines = vec!["<file_map>".to_string()];
    for file in files {
        lines.push(format!(
            "- {} [{}]",
            file.display_path,
            mode_name(&file.mode)
        ));
    }
    lines.push("</file_map>".to_string());
    lines.join("\n")
}

fn render_instructions(instructions: &str) -> String {
    format!("<instructions>\n{instructions}\n</instructions>")
}

/// Render the caller-supplied working-tree diff as a `<git_diff>` section.
fn render_git_diff(diff: &str) -> String {
    format!("<git_diff>\n{}\n</git_diff>", diff.trim_end())
}

/// Render reusable instruction blocks as numbered `<meta prompt N="title">` sections.
fn render_meta_prompts(prompts: &[MetaPrompt]) -> String {
    prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            let n = i + 1;
            format!(
                "<meta prompt {n}=\"{title}\">\n{body}\n</meta prompt {n}>",
                title = prompt.title,
                body = prompt.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Caller-supplied meta-prompts, or the named recipe's defaults when none given.
fn resolve_meta_prompts(request: &WorkspaceContextRequest) -> Vec<MetaPrompt> {
    if !request.meta_prompts.is_empty() {
        return request.meta_prompts.clone();
    }
    request
        .recipe
        .as_deref()
        .and_then(crate::recipe::recipe_by_name)
        .map(|recipe| {
            recipe
                .meta_prompts
                .iter()
                .map(|(title, body)| MetaPrompt {
                    title: (*title).to_string(),
                    body: (*body).to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn render_selected_file_cached<P: CatalogProvider>(
    provider: &P,
    selected: &SelectedFile<'_>,
    cache: &mut RenderCache,
) -> Result<RenderedFile, NerveError> {
    let key = (selected.key.clone(), selected.mode.clone());
    if let Some(rendered) = cache.entries.get(&key) {
        return Ok(rendered.clone());
    }
    let rendered = render_selected_file(provider, selected)?;
    cache.entries.insert(key, rendered.clone());
    Ok(rendered)
}

fn selection_key(entry: &CatalogEntry) -> SelectionKey {
    SelectionKey {
        root_id: entry.root_id.clone(),
        path: entry.rel_path.clone(),
    }
}

fn mode_name(mode: &SelectionMode) -> &'static str {
    match mode {
        SelectionMode::Full => "full",
        SelectionMode::Slices(_) => "slices",
        SelectionMode::CodemapOnly => "codemap_only",
    }
}
