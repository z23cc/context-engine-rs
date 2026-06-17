use super::*;
use crate::auth;

pub(super) fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .timeout_global(Some(timeout.max(Duration::from_secs(5))))
        .build()
        .into()
}

pub(super) fn http_get_json(url: &str, bearer: &str, timeout: Duration) -> Result<Value> {
    let mut response = send_get_json(url, bearer, timeout)?;
    if is_unauthorized(&response) {
        let refreshed = auth::resolve_runtime_credentials(true)?;
        response = send_get_json(url, &refreshed.access_token, timeout)?;
    }
    read_json_response(response)
}

pub(super) fn http_post_json(
    url: &str,
    bearer: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<Value> {
    let mut response = send_post_json(url, bearer, payload, timeout)?;
    if is_unauthorized(&response) {
        let refreshed = auth::resolve_runtime_credentials(true)?;
        response = send_post_json(url, &refreshed.access_token, payload, timeout)?;
    }
    read_json_response(response)
}

pub(super) fn http_post_bytes(
    url: &str,
    bearer: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut response = send_post_json(url, bearer, payload, timeout)?;
    if is_unauthorized(&response) {
        let refreshed = auth::resolve_runtime_credentials(true)?;
        response = send_post_json(url, &refreshed.access_token, payload, timeout)?;
    }
    ensure_success(&mut response)?;
    response
        .body_mut()
        .read_to_vec()
        .map_err(|err| anyhow!(err.to_string()))
}

pub(super) fn http_get_bytes(url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let agent = http_agent(timeout);
    let mut response = agent
        .get(url)
        .header("User-Agent", user_agent())
        .call()
        .map_err(|err| anyhow!(err.to_string()))?;
    ensure_success(&mut response)?;
    response
        .body_mut()
        .read_to_vec()
        .map_err(|err| anyhow!(err.to_string()))
}

pub(super) fn http_post_multipart_stt(
    url: &str,
    bearer: &str,
    file_path: &Path,
    language: Option<String>,
    use_format: bool,
    diarize: bool,
    timeout: Duration,
) -> Result<Value> {
    let mut response = send_multipart_stt(
        url,
        bearer,
        file_path,
        language.as_deref(),
        use_format,
        diarize,
        timeout,
    )?;
    if is_unauthorized(&response) {
        let refreshed = auth::resolve_runtime_credentials(true)?;
        response = send_multipart_stt(
            url,
            &refreshed.access_token,
            file_path,
            language.as_deref(),
            use_format,
            diarize,
            timeout,
        )?;
    }
    read_json_response(response)
}

fn send_get_json(
    url: &str,
    bearer: &str,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>> {
    http_agent(timeout)
        .get(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {bearer}"))
        .header("User-Agent", user_agent())
        .call()
        .map_err(|err| anyhow!(err.to_string()))
}

fn send_post_json(
    url: &str,
    bearer: &str,
    payload: &Value,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>> {
    http_agent(timeout)
        .post(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {bearer}"))
        .header("User-Agent", user_agent())
        .send_json(payload)
        .map_err(|err| anyhow!(err.to_string()))
}

fn send_multipart_stt(
    url: &str,
    bearer: &str,
    file_path: &Path,
    language: Option<&str>,
    use_format: bool,
    diarize: bool,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>> {
    use ureq::unversioned::multipart::Form;

    let mut form = Form::new().file("file", file_path)?;
    if let Some(language) = language.filter(|value| !value.is_empty()) {
        form = form.text("language", language);
    }
    form = form
        .text("format", if use_format { "true" } else { "false" })
        .text("diarize", if diarize { "true" } else { "false" });

    http_agent(timeout)
        .post(url)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {bearer}"))
        .header("User-Agent", user_agent())
        .send(form)
        .map_err(|err| anyhow!(err.to_string()))
}

pub(super) fn read_json_response(mut response: ureq::http::Response<ureq::Body>) -> Result<Value> {
    ensure_success(&mut response)?;
    let body = response.body_mut().read_to_string().unwrap_or_default();
    serde_json::from_str(&body).with_context(|| format!("invalid JSON response: {body}"))
}

pub(super) fn ensure_success(response: &mut ureq::http::Response<ureq::Body>) -> Result<()> {
    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = response.body_mut().read_to_string().unwrap_or_default();
    if status == 401 {
        bail!(
            "xAI returned HTTP 401; run `ctx-mcp auth status --refresh` or `ctx-mcp auth login xai --force`. Response: {body}"
        );
    }
    if status == 403 {
        bail!(
            "xAI returned HTTP 403; this OAuth account may lack API entitlement. Response: {body}"
        );
    }
    bail!("xAI returned HTTP {status}: {body}")
}

fn is_unauthorized(response: &ureq::http::Response<ureq::Body>) -> bool {
    response.status().as_u16() == 401
}

pub(super) fn user_agent() -> String {
    format!("ctx-mcp/{}", env!("CARGO_PKG_VERSION"))
}
