//! Shared blocking HTTP/SSE helpers used by every provider.
//!
//! This mirrors the synchronous `ureq` v3 style used elsewhere in the
//! workspace: an [`http_agent`] with sane defaults, a [`post_json`] helper for
//! non-streaming exchanges (token refresh, etc.), and [`post_sse`] which opens
//! a streaming response and yields each `data:` payload via [`SseReader`].
//! Providers are responsible for parsing the JSON inside each event.

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use nerve_core::CancelToken;
use serde_json::Value;

use super::retry::{Attempt, RetryPolicy, is_retryable_status, with_retry};
use crate::error::{AgentError, AgentResult};

/// User-Agent string sent with every request.
pub fn user_agent() -> String {
    format!("nerve-agent/{}", env!("CARGO_PKG_VERSION"))
}

/// Build a blocking HTTPS-only `ureq` agent with a global timeout.
pub fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .timeout_global(Some(timeout.max(Duration::from_secs(5))))
        .build()
        .into()
}

/// Apply a list of `(name, value)` headers to a request builder.
fn with_headers<Any>(
    mut req: ureq::RequestBuilder<Any>,
    headers: &[(String, String)],
) -> ureq::RequestBuilder<Any> {
    req = req.header("User-Agent", user_agent());
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    req
}

/// POST a JSON body and decode the JSON response (non-streaming).
///
/// Used for OAuth token exchange/refresh and other one-shot calls.
pub fn post_json(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> AgentResult<Value> {
    let req = with_headers(agent.post(url), headers).header("Accept", "application/json");
    let mut response = req
        .send_json(body)
        .map_err(|err| AgentError::Http(err.to_string()))?;

    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|err| AgentError::Http(err.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(AgentError::Http(format!("HTTP {status}: {text}")));
    }
    serde_json::from_str(&text)
        .map_err(|err| AgentError::Parse(format!("invalid JSON response: {err}: {text}")))
}

/// POST a JSON body and open a streaming Server-Sent Events response, retrying
/// transient failures (rate limits, 5xx, connection resets) with exponential
/// backoff.
///
/// Retries wrap only the connect-and-status phase — before any event is read —
/// so a retry never re-emits already-streamed deltas. A non-retryable status
/// (e.g. 400/401) fails immediately, and `cancel` aborts between tries and
/// during a backoff wait.
pub fn post_sse(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
    cancel: &CancelToken,
) -> AgentResult<SseReader> {
    with_retry(&RetryPolicy::default(), cancel, || {
        send_sse_once(agent, url, headers, body)
    })
}

/// One connect attempt: open the stream on 2xx, otherwise classify the failure
/// (transport error or non-2xx status) as retryable or fatal for [`with_retry`].
fn send_sse_once(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Attempt<SseReader> {
    let req = with_headers(agent.post(url), headers).header("Accept", "text/event-stream");
    let mut response = match req.send_json(body) {
        Ok(response) => response,
        // Transport failure (connection reset, DNS, TLS, timeout): transient.
        Err(err) => {
            return Attempt::Retry {
                error: AgentError::Http(err.to_string()),
                retry_after: None,
            };
        }
    };

    let status = response.status().as_u16();
    if (200..300).contains(&status) {
        let reader = response.into_body().into_reader();
        return Attempt::Done(SseReader {
            reader: BufReader::new(Box::new(reader)),
        });
    }

    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let text = response.body_mut().read_to_string().unwrap_or_default();
    let error = AgentError::Http(format!("HTTP {status}: {text}"));
    if is_retryable_status(status) {
        Attempt::Retry { error, retry_after }
    } else {
        Attempt::Fatal(error)
    }
}

/// A line-oriented reader over an SSE response body.
pub struct SseReader {
    reader: BufReader<Box<dyn Read>>,
}

impl SseReader {
    /// Return the next `data:` payload, or `None` at end of stream / `[DONE]`.
    ///
    /// Blank lines and comment lines (`:`) are skipped. The leading `data:`
    /// prefix and one optional space are stripped from the returned string.
    pub fn next_event(&mut self) -> AgentResult<Option<String>> {
        let mut line = String::new();
        loop {
            line.clear();
            let read = self
                .reader
                .read_line(&mut line)
                .map_err(|err| AgentError::Http(err.to_string()))?;
            if read == 0 {
                return Ok(None);
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() || trimmed.starts_with(':') {
                continue;
            }
            let Some(rest) = trimmed.strip_prefix("data:") else {
                // Non-data field (event:, id:, retry:) — skip it.
                continue;
            };
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            if payload == "[DONE]" {
                return Ok(None);
            }
            return Ok(Some(payload.to_string()));
        }
    }
}
