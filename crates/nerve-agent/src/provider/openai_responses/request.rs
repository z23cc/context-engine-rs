//! Translation of the provider-neutral [`ChatRequest`] into an OpenAI
//! Responses API request body.
//!
//! Mirrors `_to_responses_input` / `_prepare_oai_tools` from the reference
//! `GenericAgent/llmcore.py`: the system prompt becomes top-level
//! `instructions`, messages become `input` items, assistant tool calls become
//! `function_call` items, tool results become `function_call_output` items, and
//! tools become `{type:"function", name, description, parameters}` entries.

use serde_json::{Map, Value, json};

use crate::message::{ChatRequest, Message, Role, ToolCall, ToolSpec};

/// Build the JSON body for `POST /v1/responses`.
///
/// `stream` is always `true` for this provider; `instructions`, `input`,
/// `tools`, `temperature`, `max_output_tokens`, and `reasoning` are populated
/// from `req`. When `client_metadata` is `Some`, it is attached (OAuth mode).
pub(super) fn build_body(req: &ChatRequest, client_metadata: Option<Value>) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("stream".into(), json!(true));
    // The Codex backend rejects requests unless `store` is false; nerve resends
    // full history each turn, so server-side response storage is unused anyway.
    body.insert("store".into(), json!(false));
    body.insert("input".into(), Value::Array(to_input_items(&req.messages)));

    if let Some(system) = req.system.as_deref().filter(|s| !s.is_empty()) {
        body.insert("instructions".into(), json!(system));
    }
    if !req.tools.is_empty() {
        body.insert("tools".into(), Value::Array(prepare_tools(&req.tools)));
    }
    if let Some(temperature) = req.temperature {
        body.insert("temperature".into(), json!(clean_f32(temperature)));
    }
    if let Some(max_tokens) = req.max_tokens {
        body.insert("max_output_tokens".into(), json!(max_tokens));
    }
    if let Some(effort) = req.reasoning_effort.as_deref().filter(|s| !s.is_empty()) {
        body.insert("reasoning".into(), json!({ "effort": effort }));
    }
    if let Some(metadata) = client_metadata {
        body.insert("client_metadata".into(), metadata);
    }
    Value::Object(body)
}

/// Convert tool specs into Responses-style function tool entries.
///
/// `[{type:"function", name, description, parameters}]` where `parameters` is
/// the tool's JSON Schema (`input_schema`).
fn prepare_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect()
}

/// Convert conversation messages into Responses `input` items.
fn to_input_items(messages: &[Message]) -> Vec<Value> {
    let mut items = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::Tool => items.push(tool_result_item(msg)),
            _ => push_message_items(&mut items, msg),
        }
    }
    items
}

/// A `function_call_output` item keyed by the originating `call_id`.
fn tool_result_item(msg: &Message) -> Value {
    let call_id = msg.tool_call_id.clone().unwrap_or_default();
    json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": msg.content,
    })
}

/// Push a role message item, followed by any assistant `function_call` items.
fn push_message_items(items: &mut Vec<Value>, msg: &Message) {
    let role = responses_role(msg.role);
    let text_type = if msg.role == Role::Assistant {
        "output_text"
    } else {
        "input_text"
    };
    let parts = content_parts(&msg.content, text_type);
    items.push(json!({ "role": role, "content": parts }));

    for call in &msg.tool_calls {
        items.push(function_call_item(call));
    }
}

/// Map a neutral [`Role`] to the Responses role string. The Responses API uses
/// `developer` in place of `system`.
fn responses_role(role: Role) -> &'static str {
    match role {
        Role::System => "developer",
        Role::Assistant => "assistant",
        // `Tool` is handled separately; treat anything else as `user`.
        Role::User | Role::Tool => "user",
    }
}

/// Build the `content` parts array for a message. An empty message still emits a
/// single (empty-text) part so the item is well-formed.
fn content_parts(content: &str, text_type: &str) -> Vec<Value> {
    if content.is_empty() {
        return vec![json!({ "type": text_type, "text": "" })];
    }
    vec![json!({ "type": text_type, "text": content })]
}

/// A `function_call` item. The neutral [`ToolCall::arguments`] is an object; the
/// Responses API expects `arguments` as a JSON-encoded string.
fn function_call_item(call: &ToolCall) -> Value {
    json!({
        "type": "function_call",
        "call_id": call.id,
        "name": call.name,
        "arguments": arguments_to_string(&call.arguments),
    })
}

/// Convert an `f32` to the `f64` matching its shortest decimal representation,
/// so a temperature like `0.7` serializes as `0.7` rather than `0.699999988`.
fn clean_f32(value: f32) -> f64 {
    value.to_string().parse().unwrap_or(value as f64)
}

/// Encode tool-call arguments as the JSON string the Responses API expects.
/// A string value is passed through verbatim (already serialized upstream).
fn arguments_to_string(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        Value::Null => "{}".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ToolSpec;

    fn req_with(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5".into(),
            system: Some("be helpful".into()),
            messages,
            tools: Vec::new(),
            temperature: Some(0.7),
            max_tokens: Some(256),
            reasoning_effort: Some("high".into()),
        }
    }

    #[test]
    fn maps_system_to_instructions_and_scalars() {
        let body = build_body(&req_with(vec![Message::user("hi")]), None);
        assert_eq!(body["instructions"], json!("be helpful"));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["temperature"], json!(0.7));
        assert_eq!(body["max_output_tokens"], json!(256));
        assert_eq!(body["reasoning"], json!({ "effort": "high" }));
        assert!(body.get("client_metadata").is_none());
    }

    #[test]
    fn user_message_uses_input_text() {
        let body = build_body(&req_with(vec![Message::user("hello")]), None);
        let item = &body["input"][0];
        assert_eq!(item["role"], json!("user"));
        assert_eq!(item["content"][0]["type"], json!("input_text"));
        assert_eq!(item["content"][0]["text"], json!("hello"));
    }

    #[test]
    fn system_message_becomes_developer() {
        let body = build_body(&req_with(vec![Message::system("rules")]), None);
        assert_eq!(body["input"][0]["role"], json!("developer"));
        assert_eq!(body["input"][0]["content"][0]["type"], json!("input_text"));
    }

    #[test]
    fn assistant_tool_call_emits_function_call_item() {
        let mut assistant = Message::assistant("calling");
        assistant.tool_calls = vec![ToolCall {
            id: "call_1".into(),
            name: "search".into(),
            arguments: json!({ "q": "rust" }),
        }];
        let body = build_body(&req_with(vec![assistant]), None);
        // message item, then function_call item.
        assert_eq!(body["input"][0]["content"][0]["type"], json!("output_text"));
        let fc = &body["input"][1];
        assert_eq!(fc["type"], json!("function_call"));
        assert_eq!(fc["call_id"], json!("call_1"));
        assert_eq!(fc["name"], json!("search"));
        assert_eq!(fc["arguments"], json!("{\"q\":\"rust\"}"));
    }

    #[test]
    fn tool_message_becomes_function_call_output() {
        let tool = Message::tool("call_1", "search", "result text");
        let body = build_body(&req_with(vec![tool]), None);
        let item = &body["input"][0];
        assert_eq!(item["type"], json!("function_call_output"));
        assert_eq!(item["call_id"], json!("call_1"));
        assert_eq!(item["output"], json!("result text"));
    }

    #[test]
    fn tools_become_function_entries() {
        let mut req = req_with(vec![Message::user("hi")]);
        req.tools = vec![ToolSpec {
            name: "read_file".into(),
            description: "reads a file".into(),
            input_schema: json!({ "type": "object" }),
        }];
        let body = build_body(&req, None);
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert_eq!(tool["name"], json!("read_file"));
        assert_eq!(tool["description"], json!("reads a file"));
        assert_eq!(tool["parameters"], json!({ "type": "object" }));
    }

    #[test]
    fn attaches_client_metadata_when_present() {
        let metadata = json!({ "x-codex-window-id": "w:0" });
        let body = build_body(&req_with(vec![Message::user("hi")]), Some(metadata.clone()));
        assert_eq!(body["client_metadata"], metadata);
    }

    #[test]
    fn omits_optional_fields_when_absent() {
        let req = ChatRequest {
            model: "gpt-5".into(),
            system: None,
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        };
        let body = build_body(&req, None);
        assert!(body.get("instructions").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn empty_content_still_emits_part() {
        let body = build_body(&req_with(vec![Message::assistant("")]), None);
        assert_eq!(body["input"][0]["content"][0]["text"], json!(""));
        assert_eq!(body["input"][0]["content"][0]["type"], json!("output_text"));
    }

    #[test]
    fn string_arguments_pass_through_verbatim() {
        let mut assistant = Message::assistant("");
        assistant.tool_calls = vec![ToolCall {
            id: "c".into(),
            name: "t".into(),
            arguments: json!("{\"already\":\"string\"}"),
        }];
        let body = build_body(&req_with(vec![assistant]), None);
        assert_eq!(
            body["input"][1]["arguments"],
            json!("{\"already\":\"string\"}")
        );
    }
}
