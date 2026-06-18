//! Anthropic Messages API provider.
//!
//! Implements [`LlmProvider`] over the shared blocking HTTP/SSE helpers. The
//! request body and headers are built in [`wire`]; the streaming response is
//! folded into a [`ChatResponse`] by [`sse`]. Two auth modes are supported:
//! `x-api-key` for `sk-ant-` API keys and `authorization: Bearer` for
//! subscription OAuth (which also requires the Claude Code impersonation
//! system block and the OAuth/prompt-caching betas — see [`wire`]).

mod sse;
mod wire;

use std::time::Duration;

use nerve_core::CancelToken;
use serde_json::Value;

use crate::auth::{Credential, ProviderId};
use crate::error::{AgentError, AgentResult};
use crate::message::{ChatDelta, ChatRequest, ChatResponse};
use crate::provider::LlmProvider;
use crate::provider::http::{SseReader, http_agent, post_sse};

/// Global request timeout for a streaming Messages call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Provider talking to the Anthropic Messages API.
pub struct AnthropicProvider {
    credential: Credential,
}

impl AnthropicProvider {
    /// Build a provider from a resolved credential.
    pub fn new(credential: Credential) -> Self {
        Self { credential }
    }

    /// The `POST` target, `{base_url}/v1/messages`.
    ///
    /// Falls back to the provider default base URL when the credential's is
    /// blank, and tolerates a base URL that already ends in `/v1`.
    fn messages_url(&self) -> String {
        let base = self.credential.base_url.trim_end_matches('/');
        let base = if base.is_empty() {
            ProviderId::Anthropic.default_base_url()
        } else {
            base
        };
        if base.ends_with("/v1") {
            format!("{base}/messages")
        } else {
            format!("{base}/v1/messages")
        }
    }
}

impl LlmProvider for AnthropicProvider {
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
        let agent = http_agent(REQUEST_TIMEOUT);
        let headers = wire::build_headers(&self.credential);
        let body = wire::build_body(req, &self.credential);
        let mut reader = post_sse(&agent, &self.messages_url(), &headers, &body, cancel)?;
        drive_stream(&mut reader, cancel, sink)
    }
}

/// Pull SSE events to completion, folding them into a [`ChatResponse`].
///
/// Cancellation is checked before each event read; the stream ends on a
/// `message_stop` event or when the connection closes.
fn drive_stream(
    reader: &mut SseReader,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(ChatDelta),
) -> AgentResult<ChatResponse> {
    let mut builder = sse::ResponseBuilder::new();
    loop {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let Some(payload) = reader.next_event()? else {
            break;
        };
        let evt: Value = serde_json::from_str(&payload)
            .map_err(|err| AgentError::Parse(format!("invalid SSE JSON: {err}: {payload}")))?;
        if builder.apply(&evt, sink)? {
            break;
        }
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMode;

    fn provider_with_base(base: &str) -> AnthropicProvider {
        AnthropicProvider::new(Credential {
            provider: ProviderId::Anthropic,
            mode: AuthMode::ApiKey,
            access_token: "sk-ant-x".to_string(),
            refresh_token: None,
            expires_at_unix: None,
            account_id: None,
            base_url: base.to_string(),
        })
    }

    #[test]
    fn id_reflects_credential_provider() {
        assert_eq!(
            provider_with_base("https://api.anthropic.com").id(),
            ProviderId::Anthropic
        );
    }

    #[test]
    fn messages_url_appends_v1_messages() {
        assert_eq!(
            provider_with_base("https://api.anthropic.com").messages_url(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn messages_url_trims_trailing_slash() {
        assert_eq!(
            provider_with_base("https://proxy.test/").messages_url(),
            "https://proxy.test/v1/messages"
        );
    }

    #[test]
    fn messages_url_handles_existing_v1_suffix() {
        assert_eq!(
            provider_with_base("https://proxy.test/v1").messages_url(),
            "https://proxy.test/v1/messages"
        );
    }

    #[test]
    fn messages_url_falls_back_to_default_when_blank() {
        assert_eq!(
            provider_with_base("").messages_url(),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn chat_returns_cancelled_when_token_tripped() {
        let provider = provider_with_base("https://api.anthropic.com");
        let cancel = CancelToken::new();
        cancel.cancel();
        let req = ChatRequest {
            model: "claude-x".to_string(),
            system: None,
            messages: vec![crate::message::Message::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        };
        let mut sink = |_: ChatDelta| {};
        let err = provider.chat(&req, &cancel, &mut sink).unwrap_err();
        assert!(matches!(err, AgentError::Cancelled));
    }
}
