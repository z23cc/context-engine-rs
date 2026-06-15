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
pub enum CtxError {
    #[error("operation cancelled")]
    Cancelled,
    #[error("no roots configured; operation refused")]
    NoRoots,
    #[error("path is outside configured roots: {0}")]
    OutsideRoots(PathBuf),
    #[error("path traversal is not allowed: {0}")]
    PathTraversal(String),
    #[error("entry limit exceeded after {limit} entries")]
    EntryLimitExceeded { limit: usize },
    #[error("invalid regex: {0}")]
    InvalidRegex(#[from] regex::Error),
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl CtxError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
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

/// Search options independent of transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchRequest {
    pub pattern: String,
    pub mode: SearchMode,
    pub regex: bool,
    pub max_results: usize,
    pub context_lines: usize,
    pub max_content_files: usize,
    pub max_content_bytes: u64,
    pub whole_word: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            mode: SearchMode::Both,
            regex: false,
            max_results: 50,
            context_lines: 2,
            max_content_files: 2_048,
            max_content_bytes: 64 * 1024 * 1024,
            whole_word: false,
        }
    }
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
    pub path_matches: Vec<PathSearchMatch>,
    pub content_matches: Vec<ContentSearchMatch>,
    pub diagnostics: Vec<Diagnostic>,
    pub totals: SearchTotals,
}

/// Request for read_file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileRequest {
    pub path: PathBuf,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub limit: Option<usize>,
}

/// Response for read_file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadFileResponse {
    pub path: PathBuf,
    pub display_path: String,
    pub first_line: usize,
    pub last_line: usize,
    pub total_lines: usize,
    pub content: String,
}

/// Compact file tree node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTreeNode {
    pub name: String,
    pub path: String,
    pub kind: FileTreeKind,
    pub children: Vec<FileTreeNode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTreeKind {
    Directory,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTreeResponse {
    pub roots: Vec<FileTreeNode>,
    pub tree: String,
    pub roots_count: usize,
    pub was_truncated: bool,
    pub uses_legend: bool,
    pub omitted: usize,
}
