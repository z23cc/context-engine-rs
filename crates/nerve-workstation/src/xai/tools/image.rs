use super::*;
use crate::xai::DEFAULT_IMAGE_MODEL;

pub(super) fn xai_image_generate(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
) -> Result<Value> {
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
