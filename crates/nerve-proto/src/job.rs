#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::RuntimeCommand;

/// Status values used by daemon-owned runtime jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RuntimeJobStatus {
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

/// Structured error stored on failed runtime jobs.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobError {
    pub kind: String,
    pub message: String,
}

impl RuntimeJobError {
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
        }
    }
}

/// Snapshot of a runtime job owned by a transport/daemon implementation.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobSnapshot {
    pub job_id: String,
    pub status: RuntimeJobStatus,
    pub command: String,
    pub tool_name: Option<String>,
    pub created_at_ms: u64,
    pub started_at_ms: Option<u64>,
    pub updated_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub cancel_requested: bool,
    pub result: Option<Value>,
    pub error: Option<RuntimeJobError>,
}

/// Request payload for `runtime/jobs/start`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobStartRequest {
    pub job_id: Option<String>,
    pub command: RuntimeCommand,
}

/// Request payload for `runtime/jobs/get`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobGetRequest {
    pub job_id: String,
    #[serde(default = "default_true")]
    pub include_result: bool,
}

/// Request payload for `runtime/jobs/cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobCancelRequest {
    pub job_id: String,
}

/// Request payload for `runtime/jobs/list`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RuntimeJobListRequest {
    #[serde(default = "default_true")]
    pub include_terminal: bool,
    #[serde(default)]
    pub include_results: bool,
    #[serde(default = "default_job_list_limit")]
    pub limit: usize,
}

fn default_true() -> bool {
    true
}

fn default_job_list_limit() -> usize {
    100
}
