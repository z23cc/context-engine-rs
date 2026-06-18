//! OpenAI Responses API provider.
//!
//! Talks to `POST {base_url}/v1/responses` with `stream:true`, parsing the
//! resulting Server-Sent Events into a [`ChatResponse`]. The wire translation
//! lives in [`request`] and the SSE state machine in [`sse`]; this module owns
//! the HTTP plumbing, the dual authentication header modes, and the streaming
//! drive loop.
//!
//! ## Authentication modes
//! - [`AuthMode::ApiKey`]: a plain `Authorization: Bearer <key>` is sent.
//! - [`AuthMode::Oauth`] (ChatGPT/Codex subscription): in addition to the bearer
//!   token, Codex-style signaling is attached — an `originator: codex_exec`
//!   header, a `chatgpt-account-id` header when an account id is known, and a
//!   `client_metadata` object in the request body carrying
//!   `x-codex-window-id` / `x-codex-installation-id`. This mirrors the
//!   `NativeOAISession` flow in the reference `GenericAgent/llmcore.py`.

use std::time::Duration;

use nerve_core::CancelToken;
use rand::RngCore;
use rand::rngs::OsRng;
use serde_json::{Value, json};

use crate::auth::{AuthMode, Credential, ProviderId};
use crate::error::{AgentError, AgentResult};
use crate::message::{ChatDelta, ChatRequest, ChatResponse};
use crate::provider::LlmProvider;
use crate::provider::http::{http_agent, post_sse};

mod request;
mod sse;

/// `originator` header value identifying the Codex executor client.
const CODEX_ORIGINATOR: &str = "codex_exec";

/// `User-Agent` advertised in OAuth (Codex) mode, in the `codex_exec/<ver>`
/// style the Responses backend expects from a subscription client.
const CODEX_USER_AGENT: &str =
    "codex_exec/0.139.0 (Windows 10.0.26200; x86_64) unknown (codex_exec; 0.139.0)";

/// Overall HTTP timeout for a streaming Responses call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(600);

/// Provider talking to the OpenAI Responses API.
pub struct OpenAiResponsesProvider {
    credential: Credential,
    /// Stable per-instance Codex window id (`x-codex-window-id`).
    window_id: String,
    /// Stable per-instance Codex installation id (`x-codex-installation-id`).
    installation_id: String,
}

impl OpenAiResponsesProvider {
    /// Build a provider from a resolved credential.
    pub fn new(credential: Credential) -> Self {
        Self {
            credential,
            window_id: format!("{}:0", random_uuid()),
            installation_id: random_uuid(),
        }
    }

    /// The full URL of the Responses streaming endpoint.
    fn endpoint(&self) -> String {
        let base = self.credential.base_url.trim_end_matches('/');
        format!("{base}/v1/responses")
    }

    /// Whether this credential authenticates via ChatGPT/Codex OAuth.
    fn is_oauth(&self) -> bool {
        self.credential.mode == AuthMode::Oauth
    }

    /// Build the request headers for the active authentication mode.
    ///
    /// `Accept` is supplied by [`post_sse`]. The shared HTTP helper sets a
    /// baseline `User-Agent`; in OAuth mode we additionally advertise the Codex
    /// `User-Agent`, the `originator` header, and (when known) the
    /// `chatgpt-account-id` header so the request is recognized as Codex
    /// subscription traffic.
    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.credential.access_token),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ];
        if self.is_oauth() {
            headers.push(("originator".to_string(), CODEX_ORIGINATOR.to_string()));
            headers.push(("User-Agent".to_string(), CODEX_USER_AGENT.to_string()));
            if let Some(account_id) = self.account_id() {
                headers.push(("chatgpt-account-id".to_string(), account_id));
            }
        }
        headers
    }

    /// The Codex `client_metadata` body object, present only in OAuth mode.
    fn client_metadata(&self) -> Option<Value> {
        if !self.is_oauth() {
            return None;
        }
        Some(json!({
            "x-codex-window-id": self.window_id,
            "x-codex-installation-id": self.installation_id,
        }))
    }

    /// A non-empty account id for the `chatgpt-account-id` header, if known.
    fn account_id(&self) -> Option<String> {
        self.credential
            .account_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// Stream the Responses SSE feed, draining events into `sink`.
    fn stream(
        &self,
        body: &Value,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(ChatDelta),
    ) -> AgentResult<ChatResponse> {
        let agent = http_agent(HTTP_TIMEOUT);
        let mut reader = post_sse(&agent, &self.endpoint(), &self.headers(), body, cancel)?;
        let mut assembler = sse::Assembler::default();

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }
            let Some(payload) = reader.next_event()? else {
                break;
            };
            let event = parse_event(&payload)?;
            if assembler.handle_event(&event, sink)? == sse::Flow::Stop {
                break;
            }
        }
        Ok(assembler.finish())
    }
}

impl LlmProvider for OpenAiResponsesProvider {
    fn id(&self) -> ProviderId {
        self.credential.provider
    }

    fn chat(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(ChatDelta),
    ) -> AgentResult<ChatResponse> {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let body = request::build_body(req, self.client_metadata());
        self.stream(&body, cancel, sink)
    }
}

/// Decode one SSE `data:` payload into JSON, surfacing malformed events as
/// [`AgentError::Parse`].
fn parse_event(payload: &str) -> AgentResult<Value> {
    serde_json::from_str(payload)
        .map_err(|err| AgentError::Parse(format!("invalid SSE event JSON: {err}: {payload}")))
}

/// Generate a random RFC-4122-shaped (v4) UUID string without pulling in a
/// `uuid` dependency. Used for the Codex window/installation identifiers.
fn random_uuid() -> String {
    use std::fmt::Write as _;

    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1 (RFC 4122)

    let mut hex = String::with_capacity(32);
    for byte in bytes {
        // Writing to a String is infallible; the Result is discarded.
        let _ = write!(hex, "{byte:02x}");
    }
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(mode: AuthMode, account_id: Option<&str>) -> Credential {
        Credential {
            provider: ProviderId::OpenAi,
            mode,
            access_token: "tok-123".into(),
            refresh_token: None,
            expires_at_unix: None,
            account_id: account_id.map(str::to_string),
            base_url: "https://api.openai.com".into(),
        }
    }

    #[test]
    fn endpoint_appends_v1_responses_without_double_slash() {
        let mut cred = credential(AuthMode::ApiKey, None);
        cred.base_url = "https://api.openai.com/".into();
        let provider = OpenAiResponsesProvider::new(cred);
        assert_eq!(provider.endpoint(), "https://api.openai.com/v1/responses");
    }

    #[test]
    fn api_key_mode_sends_plain_bearer_only() {
        let provider = OpenAiResponsesProvider::new(credential(AuthMode::ApiKey, Some("acct_1")));
        let headers = provider.headers();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer tok-123")
        );
        assert!(headers.iter().any(|(k, _)| k == "Content-Type"));
        // No Codex signaling for API-key credentials.
        assert!(!headers.iter().any(|(k, _)| k == "originator"));
        assert!(!headers.iter().any(|(k, _)| k == "chatgpt-account-id"));
        assert!(!headers.iter().any(|(k, _)| k == "User-Agent"));
        assert!(provider.client_metadata().is_none());
    }

    #[test]
    fn oauth_mode_adds_codex_originator_and_account_header() {
        let provider = OpenAiResponsesProvider::new(credential(AuthMode::Oauth, Some("acct_9")));
        let headers = provider.headers();
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "originator" && v == CODEX_ORIGINATOR)
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "chatgpt-account-id" && v == "acct_9")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "User-Agent" && v.starts_with("codex_exec/"))
        );
    }

    #[test]
    fn oauth_mode_without_account_id_omits_account_header() {
        let provider = OpenAiResponsesProvider::new(credential(AuthMode::Oauth, None));
        let headers = provider.headers();
        assert!(headers.iter().any(|(k, _)| k == "originator"));
        assert!(!headers.iter().any(|(k, _)| k == "chatgpt-account-id"));
    }

    #[test]
    fn oauth_client_metadata_carries_codex_ids() {
        let provider = OpenAiResponsesProvider::new(credential(AuthMode::Oauth, None));
        let metadata = provider
            .client_metadata()
            .expect("oauth has client_metadata");
        let window = metadata["x-codex-window-id"].as_str().unwrap();
        assert!(window.ends_with(":0"));
        assert!(metadata["x-codex-installation-id"].is_string());
        // The window id embeds a distinct uuid from the installation id.
        assert_ne!(
            metadata["x-codex-installation-id"].as_str().unwrap(),
            window.trim_end_matches(":0")
        );
    }

    #[test]
    fn random_uuid_is_v4_shaped() {
        let id = random_uuid();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'), "version nibble must be 4");
    }

    #[test]
    fn id_reports_credential_provider() {
        let provider = OpenAiResponsesProvider::new(credential(AuthMode::ApiKey, None));
        assert_eq!(provider.id(), ProviderId::OpenAi);
    }

    #[test]
    fn parse_event_rejects_garbage() {
        assert!(parse_event("not json").is_err());
        assert!(parse_event("{\"type\":\"x\"}").is_ok());
    }
}
