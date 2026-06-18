//! xAI (Grok) chat-completions provider.
//!
//! xAI exposes an OpenAI-compatible API. This provider speaks the OpenAI Chat
//! Completions wire format (`POST {base_url}/v1/chat/completions` with
//! `stream: true`) over the shared blocking [`crate::provider::http`] helpers,
//! mirroring the synchronous `ureq`/SSE style used elsewhere in the workspace.
//!
//! Authentication is a bearer token for both API-key and OAuth credentials
//! (xAI accepts `Authorization: Bearer <token>` in either mode).

use std::collections::BTreeMap;
use std::time::Duration;

use nerve_core::CancelToken;
use serde_json::{Map, Value, json};

use crate::auth::{Credential, ProviderId};
use crate::error::{AgentError, AgentResult};
use crate::message::{
    ChatDelta, ChatRequest, ChatResponse, FinishReason, Message, Role, ToolCall, ToolSpec, Usage,
};
use crate::provider::LlmProvider;
use crate::provider::http::{SseReader, http_agent, post_sse};

/// Overall request timeout for a streaming chat completion.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Provider talking to the xAI chat-completions API.
pub struct XaiProvider {
    credential: Credential,
}

impl XaiProvider {
    /// Build a provider from a resolved credential.
    pub fn new(credential: Credential) -> Self {
        Self { credential }
    }

    /// The chat-completions endpoint for this credential's base URL.
    fn endpoint(&self) -> String {
        let base = self.credential.base_url.trim_end_matches('/');
        format!("{base}/v1/chat/completions")
    }

    /// Authorization/content headers for a chat request.
    fn headers(&self) -> Vec<(String, String)> {
        vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", self.credential.access_token),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }
}

impl LlmProvider for XaiProvider {
    fn id(&self) -> ProviderId {
        self.credential.provider
    }

    fn chat(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(ChatDelta),
    ) -> AgentResult<ChatResponse> {
        let body = build_body(req);
        let agent = http_agent(REQUEST_TIMEOUT);
        let mut reader = post_sse(&agent, &self.endpoint(), &self.headers(), &body, cancel)?;
        drive_stream(&mut reader, cancel, sink)
    }
}

/// Consume the SSE stream, forwarding deltas to `sink` and assembling the reply.
fn drive_stream(
    reader: &mut SseReader,
    cancel: &CancelToken,
    sink: &mut dyn FnMut(ChatDelta),
) -> AgentResult<ChatResponse> {
    let mut state = StreamState::default();
    loop {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let Some(payload) = reader.next_event()? else {
            break;
        };
        let event: Value = serde_json::from_str(&payload)
            .map_err(|err| AgentError::Parse(format!("invalid SSE chunk: {err}: {payload}")))?;
        if let Some(message) = stream_error_message(&event) {
            return Err(AgentError::Provider(message));
        }
        state.apply_event(&event, sink);
    }
    Ok(state.finish())
}

/// Extract a provider error embedded in a stream chunk, if present.
fn stream_error_message(event: &Value) -> Option<String> {
    let err = event.get("error")?;
    if let Some(message) = err.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    Some(err.to_string())
}

/// Build the OpenAI chat-completions request body from a [`ChatRequest`].
fn build_body(req: &ChatRequest) -> Value {
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if let Some(system) = req.system.as_deref().filter(|s| !s.is_empty()) {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in &req.messages {
        messages.push(message_to_json(message));
    }

    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(req.model.clone()));
    body.insert("messages".to_string(), Value::Array(messages));
    body.insert("stream".to_string(), Value::Bool(true));
    if let Some(temperature) = req.temperature {
        body.insert("temperature".to_string(), json!(clean_f32(temperature)));
    }
    if let Some(max_tokens) = req.max_tokens {
        body.insert("max_tokens".to_string(), json!(max_tokens));
    }
    if let Some(effort) = req.reasoning_effort.as_deref().filter(|s| !s.is_empty()) {
        body.insert(
            "reasoning_effort".to_string(),
            Value::String(effort.to_string()),
        );
    }
    if !req.tools.is_empty() {
        body.insert("tools".to_string(), tools_to_json(&req.tools));
    }
    Value::Object(body)
}

/// Convert an `f32` to the `f64` matching its shortest decimal representation,
/// so a temperature like `0.2` serializes as `0.2` rather than `0.200000003`.
fn clean_f32(value: f32) -> f64 {
    value.to_string().parse().unwrap_or(value as f64)
}

/// Translate the advertised tool specs into OpenAI `function` tool entries.
fn tools_to_json(tools: &[ToolSpec]) -> Value {
    let entries: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                },
            })
        })
        .collect();
    Value::Array(entries)
}

/// Translate one provider-neutral [`Message`] into an OpenAI chat message.
fn message_to_json(message: &Message) -> Value {
    match message.role {
        Role::Tool => json!({
            "role": "tool",
            "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
            "content": message.content,
        }),
        Role::Assistant => assistant_to_json(message),
        Role::System => json!({ "role": "system", "content": message.content }),
        Role::User => json!({ "role": "user", "content": message.content }),
    }
}

/// Build an assistant message, attaching a `tool_calls` array when present.
fn assistant_to_json(message: &Message) -> Value {
    let mut obj = Map::new();
    obj.insert("role".to_string(), Value::String("assistant".to_string()));
    // The OpenAI schema allows `content` to be null when only tool calls are
    // emitted; sending an empty string is equally accepted and simpler.
    obj.insert(
        "content".to_string(),
        Value::String(message.content.clone()),
    );
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": stringify_arguments(&call.arguments),
                    },
                })
            })
            .collect();
        obj.insert("tool_calls".to_string(), Value::Array(calls));
    }
    Value::Object(obj)
}

/// OpenAI expects tool-call `arguments` as a JSON *string*.
fn stringify_arguments(args: &Value) -> String {
    match args {
        Value::String(s) => s.clone(),
        Value::Null => "{}".to_string(),
        other => other.to_string(),
    }
}

/// A tool call being accumulated across streamed deltas, keyed by `index`.
#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
    /// Whether this slot's completed call has already been flushed to the sink.
    emitted: bool,
}

/// Accumulates streamed chat-completion deltas into a [`ChatResponse`].
#[derive(Default)]
struct StreamState {
    content: String,
    reasoning: String,
    /// Tool calls accumulated by their delta `index` (ordering preserved).
    tool_calls: BTreeMap<u64, PartialToolCall>,
    finish_reason: Option<FinishReason>,
    usage: Usage,
}

impl StreamState {
    /// Apply one decoded SSE event, forwarding any new text/tool deltas.
    fn apply_event(&mut self, event: &Value, sink: &mut dyn FnMut(ChatDelta)) {
        if let Some(usage) = event.get("usage").filter(|u| !u.is_null()) {
            self.absorb_usage(usage);
        }
        let Some(choice) = event
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(parse_finish_reason(reason));
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        self.absorb_reasoning(delta, sink);
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            self.content.push_str(text);
            sink(ChatDelta::Text(text.to_string()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            self.absorb_tool_calls(calls, sink);
        }
    }

    /// Record `reasoning_content` deltas (xAI's reasoning channel).
    fn absorb_reasoning(&mut self, delta: &Value, sink: &mut dyn FnMut(ChatDelta)) {
        if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            self.reasoning.push_str(reasoning);
            sink(ChatDelta::Reasoning(reasoning.to_string()));
        }
    }

    /// Merge a batch of streamed `tool_calls` deltas into the accumulator.
    fn absorb_tool_calls(&mut self, calls: &[Value], sink: &mut dyn FnMut(ChatDelta)) {
        for call in calls {
            let index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
            let slot = self.tool_calls.entry(index).or_default();
            if let Some(id) = call.get("id").and_then(Value::as_str)
                && !id.is_empty()
            {
                slot.id = id.to_string();
            }
            if let Some(function) = call.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str)
                    && !name.is_empty()
                {
                    slot.name = name.to_string();
                }
                if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                    slot.arguments.push_str(args);
                }
            }
        }
        self.flush_completed_calls(sink);
    }

    /// Emit any accumulated tool call that now parses as complete JSON, once.
    fn flush_completed_calls(&mut self, sink: &mut dyn FnMut(ChatDelta)) {
        for slot in self.tool_calls.values_mut() {
            if slot.emitted || slot.name.is_empty() {
                continue;
            }
            let Some(arguments) = parse_arguments(&slot.arguments) else {
                continue;
            };
            slot.emitted = true;
            sink(ChatDelta::ToolCall(ToolCall {
                id: slot.id.clone(),
                name: slot.name.clone(),
                arguments,
            }));
        }
    }

    /// Record token usage from a streamed `usage` object.
    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(input) = usage.get("prompt_tokens").and_then(Value::as_u64) {
            self.usage.input_tokens = input as u32;
        }
        if let Some(output) = usage.get("completion_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = output as u32;
        }
    }

    /// Assemble the final [`ChatResponse`], emitting any not-yet-flushed calls.
    fn finish(mut self) -> ChatResponse {
        let mut tool_calls = Vec::new();
        for slot in self.tool_calls.values() {
            if slot.name.is_empty() {
                continue;
            }
            let arguments = parse_arguments(&slot.arguments).unwrap_or_else(|| json!({}));
            tool_calls.push(ToolCall {
                id: slot.id.clone(),
                name: slot.name.clone(),
                arguments,
            });
        }
        let finish_reason = self.finish_reason.take().unwrap_or({
            if tool_calls.is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
            }
        });
        let reasoning = if self.reasoning.is_empty() {
            None
        } else {
            Some(self.reasoning)
        };
        ChatResponse {
            content: self.content,
            reasoning,
            tool_calls,
            finish_reason,
            usage: self.usage,
        }
    }
}

/// Parse accumulated tool-call argument text into a JSON value.
///
/// Returns `None` while the buffer is not yet valid JSON (mid-stream). An empty
/// buffer is treated as an empty object so argument-less calls still complete.
fn parse_arguments(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(json!({}));
    }
    serde_json::from_str(trimmed).ok()
}

/// Map an OpenAI `finish_reason` string to a [`FinishReason`].
fn parse_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolUse,
        "length" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthMode;

    fn credential(base_url: &str) -> Credential {
        Credential {
            provider: ProviderId::Xai,
            mode: AuthMode::ApiKey,
            access_token: "secret".to_string(),
            refresh_token: None,
            expires_at_unix: None,
            account_id: None,
            base_url: base_url.to_string(),
        }
    }

    fn sample_request() -> ChatRequest {
        ChatRequest {
            model: "grok-4".to_string(),
            system: Some("be terse".to_string()),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSpec {
                name: "echo".to_string(),
                description: "echoes".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            temperature: Some(0.2),
            max_tokens: Some(256),
            reasoning_effort: Some("high".to_string()),
        }
    }

    fn drain(state: &mut StreamState, raw: &str) -> Vec<ChatDelta> {
        let mut deltas = Vec::new();
        let event: Value = serde_json::from_str(raw).expect("valid json");
        state.apply_event(&event, &mut |delta| deltas.push(delta));
        deltas
    }

    #[test]
    fn id_is_credential_provider() {
        let provider = XaiProvider::new(credential("https://api.x.ai"));
        assert_eq!(provider.id(), ProviderId::Xai);
    }

    #[test]
    fn endpoint_normalizes_trailing_slash() {
        let provider = XaiProvider::new(credential("https://api.x.ai/"));
        assert_eq!(provider.endpoint(), "https://api.x.ai/v1/chat/completions");
    }

    #[test]
    fn headers_carry_bearer_token() {
        let provider = XaiProvider::new(credential("https://api.x.ai"));
        let headers = provider.headers();
        let auth = headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("Bearer secret"));
    }

    #[test]
    fn build_body_translates_request() {
        let body = build_body(&sample_request());
        assert_eq!(body["model"], json!("grok-4"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["max_tokens"], json!(256));
        assert_eq!(body["reasoning_effort"], json!("high"));

        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], json!("system"));
        assert_eq!(messages[0]["content"], json!("be terse"));
        assert_eq!(messages[1]["role"], json!("user"));
        assert_eq!(messages[1]["content"], json!("hi"));

        let tool = &body["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert_eq!(tool["function"]["name"], json!("echo"));
        assert_eq!(tool["function"]["parameters"], json!({ "type": "object" }));
    }

    #[test]
    fn assistant_tool_calls_become_tool_calls_array() {
        let message = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "echo".to_string(),
                arguments: json!({ "text": "hi" }),
            }],
            tool_call_id: None,
            name: None,
        };
        let value = message_to_json(&message);
        assert_eq!(value["role"], json!("assistant"));
        let call = &value["tool_calls"][0];
        assert_eq!(call["id"], json!("call_1"));
        assert_eq!(call["type"], json!("function"));
        assert_eq!(call["function"]["name"], json!("echo"));
        // Arguments must be a JSON string, not a nested object.
        assert_eq!(call["function"]["arguments"], json!("{\"text\":\"hi\"}"));
    }

    #[test]
    fn tool_result_message_carries_call_id() {
        let message = Message::tool("call_1", "echo", "done");
        let value = message_to_json(&message);
        assert_eq!(value["role"], json!("tool"));
        assert_eq!(value["tool_call_id"], json!("call_1"));
        assert_eq!(value["content"], json!("done"));
    }

    #[test]
    fn text_deltas_stream_and_accumulate() {
        let mut state = StreamState::default();
        let deltas = drain(&mut state, r#"{"choices":[{"delta":{"content":"Hel"}}]}"#);
        assert!(matches!(deltas.as_slice(), [ChatDelta::Text(t)] if t == "Hel"));
        drain(&mut state, r#"{"choices":[{"delta":{"content":"lo"}}]}"#);
        let response = state.finish();
        assert_eq!(response.content, "Hello");
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn reasoning_deltas_are_separated() {
        let mut state = StreamState::default();
        let deltas = drain(
            &mut state,
            r#"{"choices":[{"delta":{"reasoning_content":"think"}}]}"#,
        );
        assert!(matches!(deltas.as_slice(), [ChatDelta::Reasoning(t)] if t == "think"));
        let response = state.finish();
        assert_eq!(response.reasoning.as_deref(), Some("think"));
        assert!(response.content.is_empty());
    }

    #[test]
    fn tool_call_deltas_accumulate_by_index() {
        let mut state = StreamState::default();
        drain(
            &mut state,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"echo","arguments":"{\"a\":"}}]}}]}"#,
        );
        // Incomplete JSON: nothing emitted yet.
        let deltas = drain(
            &mut state,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}"#,
        );
        match deltas.as_slice() {
            [ChatDelta::ToolCall(call)] => {
                assert_eq!(call.id, "call_9");
                assert_eq!(call.name, "echo");
                assert_eq!(call.arguments, json!({ "a": 1 }));
            }
            other => panic!("expected a single tool call, got {other:?}"),
        }
        let response = state.finish();
        assert_eq!(response.tool_calls.len(), 1);
        // Not double-emitted by finish().
        assert_eq!(response.tool_calls[0].id, "call_9");
    }

    #[test]
    fn finish_reason_tool_calls_maps_to_tool_use() {
        let mut state = StreamState::default();
        drain(
            &mut state,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"echo","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        );
        let response = state.finish();
        assert_eq!(response.finish_reason, FinishReason::ToolUse);
        assert_eq!(response.tool_calls.len(), 1);
    }

    #[test]
    fn usage_is_recorded_from_stream() {
        let mut state = StreamState::default();
        drain(
            &mut state,
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#,
        );
        let response = state.finish();
        assert_eq!(response.usage.input_tokens, 11);
        assert_eq!(response.usage.output_tokens, 7);
    }

    #[test]
    fn length_finish_reason_is_mapped() {
        assert_eq!(parse_finish_reason("length"), FinishReason::Length);
        assert_eq!(
            parse_finish_reason("content_filter"),
            FinishReason::Other("content_filter".to_string())
        );
    }
}
