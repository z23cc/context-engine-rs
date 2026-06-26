use anyhow::{Context, Result, anyhow, bail};
use nerve_core::{RootPolicy, WorkspaceResolver};
use nerve_fs::FsWorkspaceRegistry;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub(super) fn object_payload(arguments: &Value) -> Result<Value> {
    let mut payload = arguments
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("arguments must be an object"))?;
    payload.remove("timeout_seconds");
    payload.remove("workspace");
    Ok(Value::Object(payload))
}

pub(super) fn tool_response(structured: Value, text: String) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
    })
}

pub(super) fn extract_response_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(Value::as_str)
        && !text.trim().is_empty()
    {
        return Some(text.trim().to_string());
    }
    let mut parts = Vec::new();
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if item.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        for content in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if matches!(
                content.get("type").and_then(Value::as_str),
                Some("output_text" | "text")
            ) && let Some(text) = content.get("text").and_then(Value::as_str)
                && !text.trim().is_empty()
            {
                parts.push(text.trim().to_string());
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

pub(super) fn extract_citations(value: &Value) -> Value {
    let mut citations = value.get("citations").cloned().unwrap_or_else(|| json!([]));
    let mut inline = Vec::new();
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for content in item
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for annotation in content
                .get("annotations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if annotation.get("type").and_then(Value::as_str) == Some("url_citation") {
                    inline.push(annotation.clone());
                }
            }
        }
    }
    if !inline.is_empty() {
        citations = json!({ "top_level": citations, "inline": inline });
    }
    citations
}

pub(super) fn text_or_summary(text: &str, summary: &str) -> String {
    if text.trim().is_empty() {
        summary.to_string()
    } else if text.chars().count() > 8_000 {
        let prefix: String = text.chars().take(8_000).collect();
        format!(
            "{}\n\n…truncated; full response is in structuredContent",
            prefix
        )
    } else {
        text.to_string()
    }
}

pub(super) fn required_string(value: &Value, key: &str) -> Result<String> {
    optional_string(value, key).ok_or_else(|| anyhow!("missing required `{key}`"))
}

pub(super) fn optional_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

pub(super) fn string_arg(value: &Value, key: &str, default: &str) -> String {
    optional_string(value, key).unwrap_or_else(|| default.to_string())
}

pub(super) fn workspace_policy(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
) -> Result<RootPolicy> {
    let workspace = optional_string(arguments, "workspace");
    let provider = registry.resolve_workspace(workspace.as_deref())?;
    let roots = provider
        .roots()
        .iter()
        .map(|root| root.path.clone())
        .collect();
    RootPolicy::new(roots).context("invalid workspace roots")
}

pub(super) fn resolve_workspace_read_path(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
    key: &str,
) -> Result<PathBuf> {
    let path = optional_string(arguments, key)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing required `{key}`"))?;
    workspace_policy(registry, arguments)?
        .resolve_allowed(&path)
        .map_err(Into::into)
}

pub(super) fn resolve_workspace_write_path(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
    key: &str,
) -> Result<PathBuf> {
    let path = optional_string(arguments, key)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("missing required `{key}`"))?;
    workspace_policy(registry, arguments)?
        .resolve_for_write(&path)
        .map_err(Into::into)
}

pub(super) fn optional_workspace_write_path(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
    key: &str,
) -> Result<Option<PathBuf>> {
    optional_string(arguments, key)
        .map(|path| {
            workspace_policy(registry, arguments)?
                .resolve_for_write(Path::new(&path))
                .map_err(Into::into)
        })
        .transpose()
}

pub(super) fn arguments_bool(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

pub(super) fn timeout_arg(value: &Value, key: &str, default: u64) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(default)
}

pub(super) fn bounded_usize(
    value: &Value,
    key: &str,
    default: usize,
    min: usize,
    max: usize,
) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map_or(default, |raw| raw as usize)
        .clamp(min, max)
}

pub(super) fn string_array(value: &Value, key: &str) -> Result<Vec<String>> {
    let Some(raw) = value.get(key) else {
        return Ok(Vec::new());
    };
    let array = raw
        .as_array()
        .ok_or_else(|| anyhow!("`{key}` must be an array"))?;
    array
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToOwned::to_owned)
                .ok_or_else(|| anyhow!("`{key}` entries must be non-empty strings"))
        })
        .collect()
}

pub(super) fn add_optional_string(args: &Value, target: &mut Value, key: &str) {
    if let Some(value) = optional_string(args, key) {
        target[key] = json!(value);
    }
}

pub(super) fn add_optional_bool(args: &Value, target: &mut Value, key: &str) {
    if let Some(value) = args.get(key).and_then(Value::as_bool) {
        target[key] = json!(value);
    }
}

pub(super) fn add_x_handle_filters(args: &Value, tool: &mut Value) -> Result<()> {
    let allowed = string_array(args, "allowed_x_handles")?;
    let excluded = string_array(args, "excluded_x_handles")?;
    if !allowed.is_empty() && !excluded.is_empty() {
        bail!("allowed_x_handles and excluded_x_handles cannot both be set");
    }
    if allowed.len() > 10 || excluded.len() > 10 {
        bail!("xAI x_search supports at most 10 handle filters");
    }
    if !allowed.is_empty() {
        tool["allowed_x_handles"] = json!(allowed);
    }
    if !excluded.is_empty() {
        tool["excluded_x_handles"] = json!(excluded);
    }
    Ok(())
}

pub(super) fn add_domain_filters(args: &Value, tool: &mut Value) -> Result<()> {
    let allowed = string_array(args, "allowed_domains")?;
    let excluded = string_array(args, "excluded_domains")?;
    if !allowed.is_empty() && !excluded.is_empty() {
        bail!("allowed_domains and excluded_domains cannot both be set");
    }
    if allowed.len() > 5 || excluded.len() > 5 {
        bail!("xAI web_search supports at most 5 domain filters");
    }
    if !allowed.is_empty() {
        tool["filters"] = json!({ "allowed_domains": allowed });
    }
    if !excluded.is_empty() {
        tool["filters"] = json!({ "excluded_domains": excluded });
    }
    Ok(())
}

pub(super) fn validate_date_range(
    from_date: Option<String>,
    to_date: Option<String>,
) -> Result<()> {
    if let Some(value) = from_date.as_deref() {
        validate_yyyy_mm_dd(value, "from_date")?;
    }
    if let Some(value) = to_date.as_deref() {
        validate_yyyy_mm_dd(value, "to_date")?;
    }
    if let (Some(from), Some(to)) = (from_date, to_date)
        && from > to
    {
        bail!("from_date must be on or before to_date");
    }
    Ok(())
}

pub(super) fn validate_yyyy_mm_dd(value: &str, label: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let ok = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| matches!(idx, 4 | 7) || byte.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        bail!("{label} must be YYYY-MM-DD")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_fs::{FsCatalogProvider, ScanOptions};
    use serde_json::json;
    use std::{fs, sync::Arc};

    fn registry_for(root: &Path) -> FsWorkspaceRegistry {
        let registry = FsWorkspaceRegistry::new();
        let policy = RootPolicy::new(vec![root.to_path_buf()]).expect("policy");
        registry.insert(
            "default",
            Arc::new(FsCatalogProvider::new(policy, ScanOptions::default())),
        );
        registry
    }

    #[test]
    fn local_file_paths_are_workspace_gated() {
        let root = tempfile::tempdir().expect("root tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let registry = registry_for(root.path());
        let canonical_root = root.path().canonicalize().expect("canonical root");
        fs::write(root.path().join("audio.wav"), b"test").expect("audio write");

        let output = resolve_workspace_write_path(
            &registry,
            &json!({ "output_path": "media/out.mp3" }),
            "output_path",
        )
        .expect("output path");
        assert!(output.starts_with(&canonical_root));

        let input = resolve_workspace_read_path(
            &registry,
            &json!({ "file_path": "audio.wav" }),
            "file_path",
        )
        .expect("input path");
        assert!(input.starts_with(&canonical_root));

        assert!(
            resolve_workspace_write_path(
                &registry,
                &json!({ "output_path": outside.path().join("out.mp3") }),
                "output_path",
            )
            .is_err()
        );
    }

    #[test]
    fn validates_x_search_dates() {
        assert!(validate_date_range(Some("2026-01-01".to_string()), None).is_ok());
        assert!(validate_date_range(Some("2026-1-1".to_string()), None).is_err());
        assert!(
            validate_date_range(
                Some("2026-02-01".to_string()),
                Some("2026-01-01".to_string())
            )
            .is_err()
        );
    }

    #[test]
    fn validates_x_search_handle_filters() {
        let mut tool = json!({ "type": "x_search" });
        assert!(
            add_x_handle_filters(
                &json!({ "allowed_x_handles": ["a"], "excluded_x_handles": ["b"] }),
                &mut tool,
            )
            .is_err()
        );
        let many: Vec<_> = (0..11).map(|idx| format!("h{idx}")).collect();
        assert!(add_x_handle_filters(&json!({ "allowed_x_handles": many }), &mut tool).is_err());
    }
}
