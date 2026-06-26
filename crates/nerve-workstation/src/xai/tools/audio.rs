use super::*;
use crate::xai::{DEFAULT_TTS_LANGUAGE, DEFAULT_TTS_VOICE};

pub(super) fn xai_tts(registry: &FsWorkspaceRegistry, arguments: &Value) -> Result<Value> {
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

pub(super) fn xai_transcribe(registry: &FsWorkspaceRegistry, arguments: &Value) -> Result<Value> {
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
