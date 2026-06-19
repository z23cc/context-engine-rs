//! JSON-RPC 2.0 envelope types for the runtime protocol (Protocol v3).
//!
//! Only the transport envelope is hand-rolled here: the `params`/`result`
//! payloads reuse `nerve_runtime` types (the single protocol authority). One
//! NDJSON line per message; see [`super::client`] for the read/write loop.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A JSON-RPC request we send to the daemon. `id` is always present (the daemon
/// only replies to messages that carry an `id`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RpcRequest {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl RpcRequest {
    pub(crate) fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// An inbound line from the daemon: either a response to one of our requests
/// (carries `id`) or a notification (carries `method`, no `id`). We classify on
/// the presence of `method` first, mirroring the TS client.
#[derive(Debug, Clone)]
pub(crate) enum Inbound {
    /// A response (`result` or `error`) keyed by the request `id`.
    Response { id: Value, payload: RpcResult },
    /// A notification: `method` + raw `params`.
    Notification { method: String, params: Value },
}

/// The `result`/`error` half of a JSON-RPC response.
#[derive(Debug, Clone)]
pub(crate) enum RpcResult {
    Ok(Value),
    Err(RpcError),
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RpcError {
    #[serde(default)]
    pub code: i64,
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)?;
        if let Some(data) = &self.data {
            write!(f, ": {data}")?;
        }
        Ok(())
    }
}

/// Parse one inbound NDJSON line. A line with a `method` field is a
/// notification; otherwise it must carry an `id` and a `result`/`error`.
/// Returns `Ok(None)` for blank lines so the read loop can skip them.
pub(crate) fn parse_inbound(line: &str) -> anyhow::Result<Option<Inbound>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|err| anyhow::anyhow!("invalid daemon JSON: {err}: {trimmed}"))?;
    if let Some(method) = value.get("method").and_then(Value::as_str) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        return Ok(Some(Inbound::Notification {
            method: method.to_string(),
            params,
        }));
    }
    let id = value
        .get("id")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("daemon message has neither method nor id: {trimmed}"))?;
    if let Some(error) = value.get("error") {
        let error: RpcError = serde_json::from_value(error.clone())
            .map_err(|err| anyhow::anyhow!("malformed JSON-RPC error: {err}"))?;
        return Ok(Some(Inbound::Response {
            id,
            payload: RpcResult::Err(error),
        }));
    }
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    Ok(Some(Inbound::Response {
        id,
        payload: RpcResult::Ok(result),
    }))
}

/// Key an `id` Value into the string used by the pending-request map. Daemon
/// echoes our numeric ids, but JSON numbers round-trip through serde as f64 in
/// some encoders, so normalize on the string form (matching the TS client's
/// `String(id)` keying).
pub(crate) fn id_key(id: &Value) -> String {
    match id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_line_yields_none() {
        assert!(parse_inbound("   ").expect("ok").is_none());
        assert!(parse_inbound("").expect("ok").is_none());
    }

    #[test]
    fn classifies_response_result() {
        let line = r#"{"jsonrpc":"2.0","id":7,"result":{"job":{"status":"completed"}}}"#;
        match parse_inbound(line).expect("ok").expect("some") {
            Inbound::Response { id, payload } => {
                assert_eq!(id_key(&id), "7");
                match payload {
                    RpcResult::Ok(value) => assert_eq!(value["job"]["status"], "completed"),
                    RpcResult::Err(err) => panic!("unexpected error: {err}"),
                }
            }
            Inbound::Notification { .. } => panic!("expected response"),
        }
    }

    #[test]
    fn classifies_response_error() {
        let line = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"bad params"}}"#;
        match parse_inbound(line).expect("ok").expect("some") {
            Inbound::Response {
                payload: RpcResult::Err(err),
                ..
            } => {
                assert_eq!(err.code, -32602);
                assert_eq!(err.message, "bad params");
            }
            other => panic!("expected error response, got {other:?}"),
        }
    }

    #[test]
    fn classifies_notification() {
        let line = r#"{"jsonrpc":"2.0","method":"runtime/event","params":{"type":"job_completed","job_id":"j1","eventSeq":4}}"#;
        match parse_inbound(line).expect("ok").expect("some") {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "runtime/event");
                assert_eq!(params["type"], "job_completed");
                assert_eq!(params["job_id"], "j1");
            }
            other => panic!("expected notification, got {other:?}"),
        }
    }

    #[test]
    fn request_serializes_as_jsonrpc() {
        let request = RpcRequest::new(1, "runtime/info", None);
        let value = serde_json::to_value(&request).expect("serialize");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 1);
        assert_eq!(value["method"], "runtime/info");
        assert!(value.get("params").is_none());
    }

    #[test]
    fn missing_id_and_method_is_error() {
        assert!(parse_inbound(r#"{"jsonrpc":"2.0"}"#).is_err());
    }
}
