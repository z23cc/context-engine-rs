use super::{DispatchError, edit};
use crate::edit::EditRequest;
use crate::{ReadFileRequest, ReadFileSnapMode, RepoMapRequest, SearchMode, SearchRequest};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
#[derive(Debug, Deserialize)]
pub(super) struct SemanticSearchArgs {
    pub(super) query: String,
    #[serde(default)]
    pub(super) mode: crate::SemanticSearchMode,
    #[serde(
        default = "default_semantic_max_results",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_results: usize,
    #[serde(default = "default_true")]
    pub(super) rerank: bool,
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
pub(super) fn default_semantic_max_results() -> usize {
    20
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
pub(super) fn default_true() -> bool {
    true
}

#[cfg(all(feature = "semantic", not(target_arch = "wasm32")))]
impl SemanticSearchArgs {
    pub(super) fn into_request(self) -> crate::SemanticSearchRequest {
        crate::SemanticSearchRequest {
            query: self.query,
            mode: self.mode,
            max_results: self.max_results,
            rerank: self.rerank,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct FileSearchArgs {
    pub(super) pattern: String,
    #[serde(default = "default_mode")]
    pub(super) mode: String,
    #[serde(default)]
    pub(super) regex: bool,
    #[serde(default = "default_max_results", deserialize_with = "lenient_usize")]
    pub(super) max_results: usize,
    #[serde(default = "default_context_lines", deserialize_with = "lenient_usize")]
    pub(super) context_lines: usize,
    #[serde(
        default = "default_max_content_files",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_content_files: usize,
    #[serde(
        default = "default_max_content_bytes",
        deserialize_with = "lenient_u64"
    )]
    pub(super) max_content_bytes: u64,
    #[serde(default)]
    pub(super) whole_word: bool,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    pub(super) context_before: Option<usize>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    pub(super) context_after: Option<usize>,
    #[serde(default)]
    pub(super) include: Vec<String>,
    #[serde(default)]
    pub(super) exclude: Vec<String>,
    #[serde(default)]
    pub(super) extensions: Vec<String>,
    #[serde(default = "default_output_mode")]
    pub(super) output_mode: String,
}

pub(super) fn default_output_mode() -> String {
    "content".to_string()
}

impl FileSearchArgs {
    pub(super) fn into_request(self) -> SearchRequest {
        SearchRequest {
            pattern: self.pattern,
            mode: match self.mode.as_str() {
                "path" => SearchMode::Path,
                "content" => SearchMode::Content,
                _ => SearchMode::Both,
            },
            regex: self.regex,
            max_results: self.max_results,
            context_lines: self.context_lines,
            context_before: self.context_before,
            context_after: self.context_after,
            max_content_files: self.max_content_files,
            max_content_bytes: self.max_content_bytes,
            whole_word: self.whole_word,
            include: self.include,
            exclude: self.exclude,
            extensions: self.extensions,
            output_mode: match self.output_mode.as_str() {
                "files_with_matches" | "files" => crate::OutputMode::FilesWithMatches,
                "count" => crate::OutputMode::Count,
                _ => crate::OutputMode::Content,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ReadFileArgs {
    pub(super) path: PathBuf,
    #[serde(default, alias = "offset", deserialize_with = "lenient_opt_usize")]
    pub(super) start_line: Option<usize>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    pub(super) end_line: Option<usize>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    pub(super) limit: Option<usize>,
    #[serde(default)]
    pub(super) view: Option<String>,
    #[serde(default)]
    pub(super) snap: Option<ReadFileSnapMode>,
}

impl ReadFileArgs {
    pub(super) fn into_request(self) -> ReadFileRequest {
        ReadFileRequest {
            path: self.path,
            start_line: self.start_line,
            end_line: self.end_line,
            limit: self.limit,
            snap: self.snap,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct EditArgs {
    pub(super) mode: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) edits: Vec<edit::ReplaceEdit>,
    #[serde(default)]
    pub(super) entries: Vec<edit::PatchEntry>,
    #[serde(default)]
    pub(super) patch: Option<String>,
    #[serde(
        default = "default_edit_diff_context_lines",
        deserialize_with = "lenient_usize"
    )]
    pub(super) diff_context_lines: usize,
    #[serde(default)]
    pub(super) diff_ignore_whitespace: bool,
    #[serde(default)]
    pub(super) atomic: bool,
}

impl EditArgs {
    pub(super) fn into_request_and_diff_options(
        self,
    ) -> Result<(EditRequest, super::DiffOptions, bool), DispatchError> {
        let EditArgs {
            mode,
            path,
            edits,
            entries,
            patch,
            diff_context_lines,
            diff_ignore_whitespace,
            atomic,
        } = self;
        let diff_options = super::DiffOptions {
            context_lines: diff_context_lines,
            ignore_whitespace: diff_ignore_whitespace,
        };
        let err = |detail: String| {
            DispatchError::Edit(edit::EditError::Parse {
                mode: "edit",
                detail,
            })
        };
        let request = match mode.as_str() {
            "replace" => EditRequest::Replace {
                path: path.ok_or_else(|| err("mode `replace` requires `path`".to_string()))?,
                edits,
            },
            "patch" => EditRequest::Patch {
                path: path.ok_or_else(|| err("mode `patch` requires `path`".to_string()))?,
                entries,
            },
            "apply_patch" | "apply-patch" => EditRequest::ApplyPatch {
                patch: patch
                    .ok_or_else(|| err("mode `apply_patch` requires `patch`".to_string()))?,
            },
            "hashline" => EditRequest::Hashline {
                patch: patch.ok_or_else(|| err("mode `hashline` requires `patch`".to_string()))?,
            },
            other => return Err(err(format!("unknown edit mode: {other}"))),
        };
        Ok((request, diff_options, atomic))
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct WriteArgs {
    pub(super) path: String,
    pub(super) content: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct DeleteArgs {
    pub(super) path: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct MoveArgs {
    pub(super) from: String,
    pub(super) to: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AstSearchArgs {
    #[serde(default)]
    pub(super) query: Option<String>,
    #[serde(default)]
    pub(super) pattern: Option<String>,
    #[serde(default)]
    pub(super) mode: Option<String>,
    pub(super) language: String,
    #[serde(default)]
    pub(super) paths: Vec<String>,
    #[serde(default = "default_ast_max", deserialize_with = "lenient_usize")]
    pub(super) max_results: usize,
}

pub(super) fn default_ast_max() -> usize {
    100
}

#[derive(Debug, Deserialize)]
pub(super) struct AstEditArgs {
    pub(super) path: String,
    #[serde(default)]
    pub(super) query: Option<String>,
    #[serde(default)]
    pub(super) pattern: Option<String>,
    #[serde(default)]
    pub(super) mode: Option<String>,
    pub(super) replacement: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct GitArgs {
    pub(super) op: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default)]
    pub(super) staged: bool,
    #[serde(default, rename = "ref")]
    pub(super) reference: Option<String>,
    #[serde(default = "default_git_count")]
    pub(super) count: usize,
    #[serde(default)]
    pub(super) lines: Option<String>,
}

pub(super) fn default_git_count() -> usize {
    20
}

#[derive(Debug, Deserialize)]
pub(super) struct FileTreeArgs {
    #[serde(default)]
    pub(super) mode: Option<String>,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    pub(super) max_depth: Option<usize>,
    #[serde(default)]
    pub(super) path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct CodeStructureArgs {
    pub(super) paths: Option<Vec<PathBuf>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct RepoMapArgs {
    pub(super) query: Option<String>,
    #[serde(default)]
    pub(super) seed_paths: Vec<PathBuf>,
    #[serde(
        default = "default_repo_map_max_files",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_files: usize,
}

impl RepoMapArgs {
    pub(super) fn into_request(self) -> RepoMapRequest {
        RepoMapRequest {
            query: self.query,
            seed_paths: self.seed_paths,
            max_files: self.max_files,
        }
    }
}

pub(super) fn default_repo_map_max_files() -> usize {
    20
}

#[derive(Debug, Deserialize)]
pub(super) struct NavigateArgs {
    pub(super) symbol: String,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(default)]
    pub(super) include_definitions: bool,
    #[serde(default)]
    pub(super) confident_only: bool,
    #[serde(
        default = "default_nav_max_results",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_results: usize,
}

impl NavigateArgs {
    pub(super) fn into_request(self) -> crate::navigate::NavigateRequest {
        crate::navigate::NavigateRequest {
            symbol: self.symbol,
            language: self.language,
            include_definitions: self.include_definitions,
            confident_only: self.confident_only,
            max_results: self.max_results.max(1),
        }
    }
}

pub(super) fn default_nav_max_results() -> usize {
    200
}

#[derive(Debug, Deserialize)]
pub(super) struct CallHierarchyArgs {
    pub(super) symbol: String,
    #[serde(default = "default_call_direction")]
    pub(super) direction: crate::navigate::CallDirection,
    #[serde(default)]
    pub(super) language: Option<String>,
    #[serde(
        default = "default_nav_max_results",
        deserialize_with = "lenient_usize"
    )]
    pub(super) max_results: usize,
}

pub(super) fn default_call_direction() -> crate::navigate::CallDirection {
    crate::navigate::CallDirection::Both
}

impl CallHierarchyArgs {
    pub(super) fn into_request(self) -> crate::navigate::CallHierarchyRequest {
        crate::navigate::CallHierarchyRequest {
            symbol: self.symbol,
            direction: self.direction,
            language: self.language,
            max_results: self.max_results.max(1),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct BuildContextArgs {
    pub(super) query: String,
    #[serde(deserialize_with = "lenient_usize")]
    pub(super) token_budget: usize,
    #[serde(default, deserialize_with = "lenient_opt_usize")]
    pub(super) max_files: Option<usize>,
    #[serde(default)]
    pub(super) seed_paths: Vec<PathBuf>,
}

impl BuildContextArgs {
    pub(super) fn into_request(self) -> crate::BuildContextRequest {
        crate::BuildContextRequest {
            query: self.query,
            token_budget: self.token_budget,
            max_files: self.max_files,
            seed_paths: self.seed_paths,
        }
    }
}

pub(super) fn default_mode() -> String {
    "both".to_string()
}

pub(super) fn default_max_results() -> usize {
    50
}

pub(super) fn default_context_lines() -> usize {
    2
}

pub(super) fn default_edit_diff_context_lines() -> usize {
    3
}

pub(super) fn default_max_content_files() -> usize {
    2_048
}

pub(super) fn default_max_content_bytes() -> u64 {
    64 * 1024 * 1024
}

/// Coerce a JSON value into u64, accepting integers and integer-valued strings.
/// LLM clients frequently emit numbers as strings (e.g. "130"); be forgiving.
pub(super) fn coerce_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => {
            let trimmed = s.trim();
            trimmed.parse::<u64>().ok().or_else(|| {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .filter(|f| *f >= 0.0 && f.fract() == 0.0)
                    .map(|f| f as u64)
            })
        }
        _ => None,
    }
}

pub(super) fn coerce_usize(value: &Value) -> Option<usize> {
    coerce_u64(value).and_then(|n| usize::try_from(n).ok())
}

/// Deserialize a usize, accepting an integer or an integer-valued string.
pub(super) fn lenient_usize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<usize, D::Error> {
    let value = Value::deserialize(deserializer)?;
    coerce_usize(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got {value}")))
}

/// Deserialize a u64, accepting an integer or an integer-valued string.
pub(super) fn lenient_u64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    coerce_u64(&value)
        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got {value}")))
}

/// Deserialize an Option<usize>, accepting null, an integer, or an integer string.
pub(super) fn lenient_opt_usize<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Ok(None);
    }
    coerce_usize(&value)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom(format!("expected an integer, got {value}")))
}
