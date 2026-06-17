use super::http::{http_get_bytes, http_get_json};
use super::util::*;
use super::*;

pub(super) fn poll_video(
    base_url: &str,
    bearer: &str,
    request_id: &str,
    args: &Value,
) -> Result<Value> {
    let timeout = timeout_arg(args, "timeout_seconds", 240);
    let interval = timeout_arg(args, "poll_interval_seconds", 5).max(1);
    let mut elapsed = 0;
    loop {
        let body = http_get_json(
            &format!("{base_url}/videos/{request_id}"),
            bearer,
            Duration::from_secs(30),
        )?;
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if status == "done" {
            return Ok(body);
        }
        if matches!(
            status.as_str(),
            "failed" | "error" | "expired" | "cancelled"
        ) {
            bail!("xAI video generation ended with status `{status}`: {body}");
        }
        if elapsed >= timeout {
            bail!("timed out waiting for xAI video generation (last status: {status})");
        }
        sleep(Duration::from_secs(interval));
        elapsed += interval;
    }
}

pub(super) fn video_payload(args: &Value, prompt: &str) -> Result<Value> {
    let image_url = optional_string(args, "image_url");
    let reference_images = string_array(args, "reference_image_urls")?;
    let model_default = if image_url.is_some() || !reference_images.is_empty() {
        DEFAULT_IMAGE_TO_VIDEO_MODEL
    } else {
        DEFAULT_VIDEO_MODEL
    };
    let mut payload = json!({
        "model": string_arg(args, "model", model_default),
        "prompt": prompt,
        "duration": bounded_usize(args, "duration", 8, 1, 15),
        "aspect_ratio": string_arg(args, "aspect_ratio", "16:9"),
        "resolution": string_arg(args, "resolution", "720p"),
    });
    if let Some(url) = image_url {
        payload["image"] = json!({ "url": url });
    }
    if !reference_images.is_empty() {
        if reference_images.len() > 7 {
            bail!("reference_image_urls supports at most 7 images");
        }
        payload["reference_images"] = json!(
            reference_images
                .iter()
                .map(|url| json!({ "url": url }))
                .collect::<Vec<_>>()
        );
    }
    Ok(payload)
}

pub(super) fn save_image_response(
    body: &Value,
    output_path: &Path,
    download_url: bool,
) -> Result<()> {
    let first = body
        .get("data")
        .and_then(Value::as_array)
        .and_then(|data| data.first())
        .ok_or_else(|| anyhow!("xAI returned no image data"))?;
    if let Some(b64) = first.get("b64_json").and_then(Value::as_str) {
        let bytes = STANDARD
            .decode(b64.as_bytes())
            .context("invalid b64_json image data")?;
        write_bytes(output_path, &bytes)?;
        return Ok(());
    }
    if let Some(url) = first.get("url").and_then(Value::as_str) {
        if download_url {
            let bytes = http_get_bytes(url, Duration::from_secs(120))?;
            write_bytes(output_path, &bytes)?;
            return Ok(());
        }
        bail!("xAI returned image URL but download_url=false: {url}");
    }
    bail!("xAI image response did not include b64_json or url")
}

pub(super) fn redact_image_response(body: &Value) -> Value {
    let mut redacted = body.clone();
    for item in redacted
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(object) = item.as_object_mut()
            && let Some(Value::String(b64)) = object.get("b64_json")
        {
            object.insert(
                "b64_json".to_string(),
                json!(format!("[redacted: {} base64 chars]", b64.len())),
            );
        }
    }
    redacted
}

pub(super) fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
}
