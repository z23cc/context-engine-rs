use super::*;

pub(super) fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .timeout_global(Some(timeout.max(Duration::from_secs(5))))
        .build()
        .into()
}

pub(super) fn http_get_json(url: &str, timeout: Duration) -> Result<Value> {
    let agent = http_agent(timeout);
    let response = agent
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|err| anyhow!(err.to_string()))?;
    read_json_response(response)
}

pub(super) fn http_post_form_json(
    url: &str,
    form: &[(&str, &str)],
    timeout: Duration,
) -> Result<Value> {
    let agent = http_agent(timeout);
    let response = agent
        .post(url)
        .header("Accept", "application/json")
        .send_form(form.iter().copied())
        .map_err(|err| anyhow!(err.to_string()))?;
    read_json_response(response)
}

pub(super) fn read_json_response(mut response: ureq::http::Response<ureq::Body>) -> Result<Value> {
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    if status == 403 {
        bail!(
            "HTTP 403: xAI OAuth account is not authorized for API access; use API-key provider if available or upgrade the subscription. Response: {body}"
        );
    }
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("invalid JSON response: {body}"))
}
