use crate::auth;
use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ctx_core::{RootPolicy, WorkspaceRegistry, WorkspaceResolver};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    thread::sleep,
    time::Duration,
};

const DEFAULT_CHAT_MODEL: &str = "grok-build-0.1";
const DEFAULT_X_SEARCH_MODEL: &str = "grok-4.20-reasoning";
const DEFAULT_WEB_SEARCH_MODEL: &str = "grok-build-0.1";
const DEFAULT_IMAGE_MODEL: &str = "grok-imagine-image";
const DEFAULT_VIDEO_MODEL: &str = "grok-imagine-video";
const DEFAULT_IMAGE_TO_VIDEO_MODEL: &str = "grok-imagine-video-1.5-preview";
const DEFAULT_TTS_VOICE: &str = "eve";
const DEFAULT_TTS_LANGUAGE: &str = "en";

mod http;
mod media;
mod specs;
mod util;

use http::*;
use media::*;
use util::*;

#[must_use]
pub(super) fn tool_specs() -> Vec<Value> {
    specs::tool_specs()
}

pub(super) fn handle_tool_call(
    registry: &WorkspaceRegistry,
    params: &Value,
) -> Result<Option<Value>> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let response = match name {
        "xai_models" => xai_models(&arguments),
        "xai_responses" => xai_responses(&arguments),
        "xai_x_search" => xai_x_search(&arguments),
        "xai_web_search" => xai_web_search(&arguments),
        "xai_image_generate" => xai_image_generate(registry, &arguments),
        "xai_tts" => xai_tts(registry, &arguments),
        "xai_transcribe" => xai_transcribe(registry, &arguments),
        "xai_video_generate" => xai_video_generate(registry, &arguments),
        _ => return Ok(None),
    }?;
    Ok(Some(response))
}

fn xai_models(_arguments: &Value) -> Result<Value> {
    let creds = auth::resolve_runtime_credentials(false)?;
    let url = format!("{}/models", creds.base_url);
    let body = http_get_json(&url, &creds.access_token, Duration::from_secs(30))?;
    Ok(tool_response(
        json!({ "provider": "xai-oauth", "base_url": creds.base_url, "models": body }),
        "xAI models fetched".to_string(),
    ))
}

fn xai_responses(arguments: &Value) -> Result<Value> {
    let creds = auth::resolve_runtime_credentials(false)?;
    let mut payload = object_payload(arguments)?;
    payload
        .as_object_mut()
        .expect("object")
        .entry("model".to_string())
        .or_insert_with(|| json!(DEFAULT_CHAT_MODEL));
    let timeout = timeout_arg(arguments, "timeout_seconds", 180);
    let body = http_post_json(
        &format!("{}/responses", creds.base_url),
        &creds.access_token,
        &payload,
        Duration::from_secs(timeout),
    )?;
    let text = extract_response_text(&body).unwrap_or_else(|| "xAI response returned".to_string());
    Ok(tool_response(
        json!({ "provider": "xai-oauth", "base_url": creds.base_url, "response": body }),
        text,
    ))
}

fn xai_x_search(arguments: &Value) -> Result<Value> {
    let query = required_string(arguments, "query")?;
    validate_date_range(
        optional_string(arguments, "from_date"),
        optional_string(arguments, "to_date"),
    )?;
    let creds = auth::resolve_runtime_credentials(false)?;
    let mut tool_def = json!({ "type": "x_search" });
    add_x_handle_filters(arguments, &mut tool_def)?;
    add_optional_string(arguments, &mut tool_def, "from_date");
    add_optional_string(arguments, &mut tool_def, "to_date");
    add_optional_bool(arguments, &mut tool_def, "enable_image_understanding");
    add_optional_bool(arguments, &mut tool_def, "enable_video_understanding");
    let model = string_arg(arguments, "model", DEFAULT_X_SEARCH_MODEL);
    let payload = json!({
        "model": model,
        "input": [{ "role": "user", "content": query }],
        "tools": [tool_def],
        "store": false,
    });
    let body = http_post_json(
        &format!("{}/responses", creds.base_url),
        &creds.access_token,
        &payload,
        Duration::from_secs(timeout_arg(arguments, "timeout_seconds", 180)),
    )?;
    let text = extract_response_text(&body).unwrap_or_default();
    let citations = extract_citations(&body);
    Ok(tool_response(
        json!({
            "provider": "xai-oauth",
            "base_url": creds.base_url,
            "model": model,
            "answer": text,
            "citations": citations,
            "raw": body,
        }),
        text_or_summary(&text, "xAI X search completed"),
    ))
}

fn xai_web_search(arguments: &Value) -> Result<Value> {
    let query = required_string(arguments, "query")?;
    let limit = bounded_usize(arguments, "limit", 5, 1, 100);
    let creds = auth::resolve_runtime_credentials(false)?;
    let mut web_tool = json!({ "type": "web_search" });
    add_domain_filters(arguments, &mut web_tool)?;
    let prompt = format!(
        "Search the web for this query and return up to {limit} concise results as JSON with fields title, url, description, position. Query: {query}"
    );
    let model = string_arg(arguments, "model", DEFAULT_WEB_SEARCH_MODEL);
    let payload = json!({
        "model": model,
        "input": [{ "role": "user", "content": prompt }],
        "tools": [web_tool],
        "include": ["no_inline_citations"],
    });
    let body = http_post_json(
        &format!("{}/responses", creds.base_url),
        &creds.access_token,
        &payload,
        Duration::from_secs(timeout_arg(arguments, "timeout_seconds", 90)),
    )?;
    let text = extract_response_text(&body).unwrap_or_default();
    Ok(tool_response(
        json!({
            "provider": "xai-oauth",
            "base_url": creds.base_url,
            "model": model,
            "answer": text,
            "citations": extract_citations(&body),
            "raw": body,
        }),
        text_or_summary(&text, "xAI web search completed"),
    ))
}

fn xai_image_generate(registry: &WorkspaceRegistry, arguments: &Value) -> Result<Value> {
    let prompt = required_string(arguments, "prompt")?;
    let output_path = resolve_workspace_write_path(registry, arguments, "output_path")?;
    let creds = auth::resolve_runtime_credentials(false)?;
    let payload = json!({
        "model": string_arg(arguments, "model", DEFAULT_IMAGE_MODEL),
        "prompt": prompt,
        "aspect_ratio": string_arg(arguments, "aspect_ratio", "1:1"),
        "resolution": string_arg(arguments, "resolution", "1k"),
    });
    let body = http_post_json(
        &format!("{}/images/generations", creds.base_url),
        &creds.access_token,
        &payload,
        Duration::from_secs(timeout_arg(arguments, "timeout_seconds", 120)),
    )?;
    save_image_response(
        &body,
        &output_path,
        arguments_bool(arguments, "download_url", true),
    )?;
    Ok(tool_response(
        json!({
            "provider": "xai-oauth",
            "base_url": creds.base_url,
            "output_path": output_path,
            "raw": redact_image_response(&body),
        }),
        format!("xAI image saved to {}", output_path.display()),
    ))
}

fn xai_tts(registry: &WorkspaceRegistry, arguments: &Value) -> Result<Value> {
    let text = required_string(arguments, "text")?;
    let output_path = resolve_workspace_write_path(registry, arguments, "output_path")?;
    let creds = auth::resolve_runtime_credentials(false)?;
    let mut payload = json!({
        "text": text,
        "voice_id": string_arg(arguments, "voice_id", DEFAULT_TTS_VOICE),
        "language": string_arg(arguments, "language", DEFAULT_TTS_LANGUAGE),
    });
    if let Some(format) = arguments.get("output_format") {
        payload["output_format"] = format.clone();
    }
    let bytes = http_post_bytes(
        &format!("{}/tts", creds.base_url),
        &creds.access_token,
        &payload,
        Duration::from_secs(timeout_arg(arguments, "timeout_seconds", 60)),
    )?;
    write_bytes(&output_path, &bytes)?;
    Ok(tool_response(
        json!({
            "provider": "xai-oauth",
            "base_url": creds.base_url,
            "output_path": output_path,
            "bytes": bytes.len(),
        }),
        format!("xAI TTS audio saved to {}", output_path.display()),
    ))
}

fn xai_transcribe(registry: &WorkspaceRegistry, arguments: &Value) -> Result<Value> {
    let file_path = resolve_workspace_read_path(registry, arguments, "file_path")?;
    let creds = auth::resolve_runtime_credentials(false)?;
    let response = http_post_multipart_stt(
        &format!("{}/stt", creds.base_url),
        &creds.access_token,
        &file_path,
        optional_string(arguments, "language"),
        arguments_bool(arguments, "format", true),
        arguments_bool(arguments, "diarize", false),
        Duration::from_secs(timeout_arg(arguments, "timeout_seconds", 120)),
    )?;
    let transcript = response
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(tool_response(
        json!({
            "provider": "xai-oauth",
            "base_url": creds.base_url,
            "transcript": transcript,
            "raw": response,
        }),
        text_or_summary(&transcript, "xAI transcription completed"),
    ))
}

fn xai_video_generate(registry: &WorkspaceRegistry, arguments: &Value) -> Result<Value> {
    let prompt = required_string(arguments, "prompt")?;
    let output_path = optional_workspace_write_path(registry, arguments, "output_path")?;
    let creds = auth::resolve_runtime_credentials(false)?;
    let payload = video_payload(arguments, &prompt)?;
    let submit = http_post_json(
        &format!("{}/videos/generations", creds.base_url),
        &creds.access_token,
        &payload,
        Duration::from_secs(60),
    )?;
    let request_id = submit
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("xAI video response did not include request_id"))?
        .to_string();
    let poll = poll_video(&creds.base_url, &creds.access_token, &request_id, arguments)?;
    let video_url = poll
        .get("video")
        .and_then(|video| video.get("url"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if output_path.is_some() && video_url.is_none() {
        bail!("xAI video completed without video.url; cannot save requested output_path");
    }
    if let (Some(path), Some(url)) = (&output_path, &video_url) {
        let bytes = http_get_bytes(url, Duration::from_secs(120))?;
        write_bytes(path, &bytes)?;
    }
    Ok(tool_response(
        json!({
            "provider": "xai-oauth",
            "base_url": creds.base_url,
            "request_id": request_id,
            "video_url": video_url,
            "output_path": output_path,
            "raw": poll,
        }),
        match (&output_path, &video_url) {
            (Some(path), _) => format!("xAI video saved to {}", path.display()),
            (_, Some(url)) => format!("xAI video generated: {url}"),
            _ => "xAI video generation completed".to_string(),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctx_core::{FsCatalogProvider, ScanOptions};
    use std::{fs, sync::Arc};

    fn registry_for(root: &Path) -> WorkspaceRegistry {
        let registry = WorkspaceRegistry::new();
        let policy = RootPolicy::new(vec![root.to_path_buf()]).expect("policy");
        registry.insert(
            "default",
            Arc::new(FsCatalogProvider::new(policy, ScanOptions::default())),
        );
        registry
    }

    #[test]
    fn lists_xai_tools() {
        let tools = tool_specs();
        let names: Vec<_> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"xai_responses"));
        assert!(names.contains(&"xai_x_search"));
        assert!(names.contains(&"xai_image_generate"));
    }

    #[test]
    fn responses_payload_strips_mcp_local_arguments() {
        let payload = object_payload(&json!({
            "model": "grok-test",
            "input": "hello",
            "timeout_seconds": 1,
            "workspace": "default",
        }))
        .expect("payload");
        assert_eq!(payload["model"], json!("grok-test"));
        assert_eq!(payload["input"], json!("hello"));
        assert!(payload.get("timeout_seconds").is_none());
        assert!(payload.get("workspace").is_none());
    }

    #[test]
    fn unknown_tool_is_not_claimed() {
        let registry = WorkspaceRegistry::new();
        let result = handle_tool_call(
            &registry,
            &json!({ "name": "file_search", "arguments": {} }),
        )
        .expect("dispatch");
        assert!(result.is_none());
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

    #[test]
    fn redacts_image_base64_from_structured_response() {
        let redacted = redact_image_response(&json!({ "data": [{ "b64_json": "abcdef" }] }));
        assert_eq!(
            redacted["data"][0]["b64_json"],
            json!("[redacted: 6 base64 chars]")
        );
    }

    #[test]
    fn video_payload_switches_default_for_image() {
        let payload = video_payload(
            &json!({ "image_url": "https://example.com/image.png" }),
            "animate it",
        )
        .expect("payload");
        assert_eq!(payload["model"], json!(DEFAULT_IMAGE_TO_VIDEO_MODEL));
        assert_eq!(
            payload["image"]["url"],
            json!("https://example.com/image.png")
        );
    }
}
