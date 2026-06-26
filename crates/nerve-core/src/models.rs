//! Shared serializable models returned by tools.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A configured root that is allowed for cataloging and reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRef {
    pub id: String,
    pub path: PathBuf,
}

/// One file entry in an immutable catalog snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub root_id: String,
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub size: u64,
}

/// Non-fatal catalog diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub path: Option<PathBuf>,
    pub message: String,
}

/// Error type for library operations.
#[derive(Debug, thiserror::Error)]
pub enum NerveError {
    #[error("operation cancelled")]
    Cancelled,
    #[error("no roots configured; operation refused")]
    NoRoots,
    #[error("ambiguous: specify workspace")]
    AmbiguousWorkspace,
    #[error("unknown workspace: {0}")]
    UnknownWorkspace(String),
    #[error("manage_workspaces requires a workspace registry")]
    ManageWorkspacesUnsupported,
    #[error("manage_workspaces requires workspace name")]
    MissingWorkspaceName,
    #[error("path is outside configured roots: {0}")]
    OutsideRoots(PathBuf),
    #[error("path traversal is not allowed: {0}")]
    PathTraversal(String),
    #[deprecated(note = "catalog scans now truncate with diagnostics instead of erroring")]
    #[error("entry limit exceeded after {limit} entries")]
    EntryLimitExceeded { limit: usize },
    #[error("invalid regex: {0}")]
    InvalidRegex(#[from] regex::Error),
    #[error("invalid glob: {0}")]
    InvalidGlob(#[from] globset::Error),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("filesystem writes are not supported by this provider")]
    WritesUnsupported,
    #[error("atomic batch unsupported by this provider")]
    AtomicBatchUnsupported,
    #[error(
        "atomic batch failed: {detail}; applied paths: {applied_paths:?}; rollback failed paths: {rollback_failed_paths:?}"
    )]
    AtomicBatchFailed {
        detail: String,
        applied_paths: Vec<PathBuf>,
        rollback_failed_paths: Vec<PathBuf>,
    },
}

impl NerveError {
    /// Build an [`NerveError::Io`] from a path + source error. `pub` so the
    /// host-side `nerve-fs` provider (which owns all real filesystem I/O) can
    /// construct kernel errors through the same constructor.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

/// Search mode for file_search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Path,
    Content,
    Both,
}

/// Output mode for content search, mirroring ripgrep / Claude Code's Grep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputMode {
    /// Full content matches with line/column/context (default).
    #[default]
    Content,
    /// Only the set of files that contain a match (paths + counts).
    FilesWithMatches,
    /// Per-file match counts only.
    Count,
}

/// Search options independent of transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub pattern: String,
    pub mode: SearchMode,
    pub regex: bool,
    pub max_results: usize,
    pub context_lines: usize,
    /// Lines of context before a match; `None` falls back to `context_lines`.
    pub context_before: Option<usize>,
    /// Lines of context after a match; `None` falls back to `context_lines`.
    pub context_after: Option<usize>,
    pub max_content_files: usize,
    pub max_content_bytes: u64,
    pub whole_word: bool,
    /// Glob whitelist on the relative path (file matches if it matches ANY).
    pub include: Vec<String>,
    /// Glob blacklist on the relative path (file dropped if it matches ANY).
    pub exclude: Vec<String>,
    /// Extension whitelist (without dot, case-insensitive), e.g. ["rs", "ts"].
    pub extensions: Vec<String>,
    pub output_mode: OutputMode,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            mode: SearchMode::Both,
            regex: false,
            max_results: 50,
            context_lines: 2,
            context_before: None,
            context_after: None,
            max_content_files: 2_048,
            max_content_bytes: 64 * 1024 * 1024,
            whole_word: false,
            include: Vec::new(),
            exclude: Vec::new(),
            extensions: Vec::new(),
            output_mode: OutputMode::Content,
        }
    }
}

/// A file with its content-match count, returned for `files_with_matches`/`count`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMatchCount {
    pub root_id: String,
    pub path: String,
    pub display_path: String,
    pub count: usize,
}

/// A path match returned by file_search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSearchMatch {
    pub root_id: String,
    pub path: String,
    pub display_path: String,
    pub score: i64,
}

/// A content match returned by file_search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentSearchMatch {
    pub root_id: String,
    pub path: String,
    pub display_path: String,
    pub score: i64,
    pub line: usize,
    pub column: usize,
    pub text: String,
    pub context: Vec<LineContext>,
}

/// One line of context around a content match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineContext {
    pub line: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Path,
    Content,
}

/// Internal search hit used for global sorting and max-result capping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchHit {
    Path(PathSearchMatch),
    Content(ContentSearchMatch),
}

impl SearchHit {
    #[must_use]
    pub fn score(&self) -> i64 {
        match self {
            Self::Path(hit) => hit.score,
            Self::Content(hit) => hit.score,
        }
    }

    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Path(hit) => &hit.path,
            Self::Content(hit) => &hit.path,
        }
    }

    #[must_use]
    pub fn line(&self) -> Option<usize> {
        match self {
            Self::Path(_) => None,
            Self::Content(hit) => Some(hit.line),
        }
    }

    #[must_use]
    pub fn kind(&self) -> MatchKind {
        match self {
            Self::Path(_) => MatchKind::Path,
            Self::Content(_) => MatchKind::Content,
        }
    }
}

/// Telemetry fields are part of the contract, even for tiny responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchTotals {
    pub scanned_files: usize,
    pub path_matches: usize,
    pub content_matches: usize,
    pub omitted: usize,
    pub content_files_scanned: usize,
    pub content_bytes_scanned: u64,
    pub binary_files_skipped: usize,
    pub content_file_limit: usize,
    pub content_byte_limit: u64,
    pub totals_are_lower_bound: bool,
    pub budget: SearchBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchBudget {
    pub max_results: usize,
    pub max_content_files: usize,
    pub max_content_bytes: u64,
    pub exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub generation: u64,
    pub path_matches: Vec<PathSearchMatch>,
    pub content_matches: Vec<ContentSearchMatch>,
    /// Populated for `files_with_matches` / `count` output modes (instead of
    /// `content_matches`); empty for the default `content` mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_files: Vec<FileMatchCount>,
    pub diagnostics: Vec<Diagnostic>,
    pub totals: SearchTotals,
}

/// Optional syntactic-boundary snapping mode for read_file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadFileSnapMode {
    None,
    Block,
}

/// Request for read_file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: PathBuf,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap: Option<ReadFileSnapMode>,
}

/// Syntactic-boundary snapping metadata for read_file responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileSnapMetadata {
    pub mode: ReadFileSnapMode,
    pub applied: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub requested_first_line: usize,
    pub requested_last_line: usize,
    pub returned_first_line: usize,
    pub returned_last_line: usize,
    pub boundary_lines: Vec<usize>,
}

/// Response for read_file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileResponse {
    pub path: PathBuf,
    pub display_path: String,
    pub first_line: usize,
    pub last_line: usize,
    pub total_lines: usize,
    /// File body. Not serialized into `structuredContent`: it is already the
    /// tool's `content[].text`, so emitting it twice would double the payload.
    #[serde(default, skip_serializing)]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snap: Option<ReadFileSnapMetadata>,
}

/// Compact file tree node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTreeNode {
    pub name: String,
    pub path: String,
    pub kind: FileTreeKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<FileTreeMarker>,
    pub children: Vec<FileTreeNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTreeKind {
    Directory,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTreeMarker {
    Selected,
    Codemap,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTreeResponse {
    /// In-memory nested tree used to render `tree`. Not serialized: the ASCII
    /// `tree` string conveys the same structure far more compactly, so emitting
    /// the nested form would only bloat `structuredContent` for clients.
    #[serde(default, skip_serializing)]
    pub roots: Vec<FileTreeNode>,
    pub tree: String,
    pub roots_count: usize,
    pub was_truncated: bool,
    pub uses_legend: bool,
    pub omitted: usize,
    /// Explains any `auto`-mode degradation or truncation (depth/folders/budget).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}
