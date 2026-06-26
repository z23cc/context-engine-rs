use super::*;

pub(super) fn xai_video_generate(
    registry: &FsWorkspaceRegistry,
    arguments: &Value,
) -> Result<Value> {
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
