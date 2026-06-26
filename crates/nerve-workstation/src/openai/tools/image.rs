use super::*;
use crate::openai::API_IMAGE_MODEL;

pub(super) fn openai_image_generate(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
) -> Result<Value> {
    let prompt = required_string(arguments, "prompt")?;
    let output_path = resolve_workspace_write_path(registry, arguments, "output_path")?;
    let creds = auth::resolve_openai_codex_credentials(false)?;
    let payload = image_payload(arguments, &prompt)?;
    let sse = http_post_sse(
        &format!("{}/responses", creds.base_url),
        &creds,
        &payload,
        Duration::from_secs(timeout_arg(arguments, "timeout_seconds", 300)),
    )?;
    save_image_sse(&sse, &output_path)?;
    Ok(tool_response(
        json!({
            "provider": "openai-codex-oauth",
            "base_url": creds.base_url,
            "output_path": output_path,
            "model": API_IMAGE_MODEL,
        }),
        format!("OpenAI image saved to {}", output_path.display()),
    ))
}
