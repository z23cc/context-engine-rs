//! Transport-neutral runtime for composing context-engine capabilities.
//!
//! `nerve-core` owns the repository/catalog operations. This crate owns the
//! runtime seam above it: a resolver plus optional capability adapters. CLI,
//! MCP, TUI, and future hosts should depend on this seam instead of duplicating
//! dispatch logic.

pub mod adapter;
pub mod command;
pub mod error;
pub mod event;
pub mod flow;
pub mod job;
pub mod protocol;
#[doc(hidden)]
pub mod protocol_codegen;
pub mod runtime;

mod tool_spec;

pub use adapter::{RiskTier, RuntimeToolAdapter, ToolCapability};
pub use command::{
    ApprovalMode, DelegateAutonomy, RUNTIME_COMMAND_NAMES, RuntimeCommand, SessionApprovalDecision,
};
pub use error::RuntimeError;
pub use event::{AgentEventKind, AuthEventKind, RuntimeEvent};
pub use flow::{
    BudgetSpec, ContextSplit, FailPolicy, Join, Step, Strategy, TaskTemplate, WorkerRef,
    WorkflowDef,
};
pub use job::{
    RuntimeJobCancelRequest, RuntimeJobError, RuntimeJobGetRequest, RuntimeJobListRequest,
    RuntimeJobSnapshot, RuntimeJobStartRequest, RuntimeJobStatus,
};
pub use runtime::Runtime;
pub use tool_spec::RuntimeToolSpec;

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::{HostFile, MemoryCatalogProvider, WorkspaceRegistry};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    type TestRuntime = Runtime<WorkspaceRegistry<MemoryCatalogProvider>>;

    struct AdapterTool;

    impl RuntimeToolAdapter<WorkspaceRegistry<MemoryCatalogProvider>> for AdapterTool {
        fn tool_specs(&self) -> Vec<Value> {
            vec![json!({
                "name": "adapter_tool",
                "description": "test adapter tool",
                "inputSchema": { "type": "object", "properties": {} }
            })]
        }

        fn handle_tool_call(
            &self,
            _resolver: &WorkspaceRegistry<MemoryCatalogProvider>,
            params: &Value,
        ) -> Result<Option<Value>, RuntimeError> {
            let name = params.get("name").and_then(Value::as_str);
            if name == Some("adapter_tool") {
                return Ok(Some(json!({ "adapter": true })));
            }
            Ok(None)
        }
    }

    struct DuplicateFileSearchSpec;

    impl RuntimeToolAdapter<WorkspaceRegistry<MemoryCatalogProvider>> for DuplicateFileSearchSpec {
        fn tool_specs(&self) -> Vec<Value> {
            vec![json!({
                "name": "file_search",
                "description": "duplicate should be skipped",
                "inputSchema": { "type": "object", "properties": {} }
            })]
        }

        fn handle_tool_call(
            &self,
            _resolver: &WorkspaceRegistry<MemoryCatalogProvider>,
            _params: &Value,
        ) -> Result<Option<Value>, RuntimeError> {
            Ok(None)
        }
    }

    struct ShadowFileSearch;

    impl RuntimeToolAdapter<WorkspaceRegistry<MemoryCatalogProvider>> for ShadowFileSearch {
        fn tool_specs(&self) -> Vec<Value> {
            Vec::new()
        }

        fn handle_tool_call(
            &self,
            _resolver: &WorkspaceRegistry<MemoryCatalogProvider>,
            params: &Value,
        ) -> Result<Option<Value>, RuntimeError> {
            let name = params.get("name").and_then(Value::as_str);
            if name == Some("file_search") {
                return Ok(Some(json!({ "shadowed": true })));
            }
            Ok(None)
        }
    }

    fn runtime() -> TestRuntime {
        let registry = WorkspaceRegistry::new();
        let provider = MemoryCatalogProvider::new(vec![HostFile::new("notes.txt", "alpha beta\n")])
            .expect("memory provider");
        registry.insert("default", Arc::new(provider));
        Runtime::new(registry)
    }

    fn arguments(value: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(value).expect("arguments object")
    }

    #[test]
    fn tool_specs_append_adapter_specs() {
        let runtime = runtime().with_adapter(AdapterTool);
        let specs = runtime.tool_specs();
        let names: Vec<_> = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"file_search"));
        assert!(names.contains(&"adapter_tool"));
    }

    #[test]
    fn adapter_can_claim_tool_call() {
        let runtime = runtime().with_adapter(AdapterTool);
        let response = runtime
            .handle_tool_call(&json!({ "name": "adapter_tool", "arguments": {} }))
            .expect("adapter response");
        assert_eq!(response["adapter"], true);
    }

    #[test]
    fn tool_specs_skip_duplicate_names() {
        let runtime = runtime().with_adapter(DuplicateFileSearchSpec);
        let specs = runtime.tool_specs();
        let count = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .filter(|tool| tool.get("name").and_then(Value::as_str) == Some("file_search"))
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn unclaimed_tool_call_falls_back_to_core() {
        let runtime = runtime().with_adapter(AdapterTool);
        let response = runtime
            .handle_tool_call(&json!({
                "name": "file_search",
                "arguments": { "pattern": "alpha", "mode": "content" }
            }))
            .expect("core response");
        assert_eq!(
            response["structuredContent"]["content_matches"][0]["path"],
            "notes.txt"
        );
    }

    #[test]
    fn adapters_precede_core_dispatch() {
        let runtime = runtime().with_adapter(ShadowFileSearch);
        let response = runtime
            .handle_tool_call(&json!({ "name": "file_search", "arguments": {} }))
            .expect("adapter response");
        assert_eq!(response["shadowed"], true);
    }

    #[test]
    fn missing_tool_name_uses_core_error_when_no_adapter_claims() {
        let runtime = runtime().with_adapter(AdapterTool);
        let error = runtime
            .handle_tool_call(&json!({ "arguments": {} }))
            .expect_err("missing name should fail");
        assert!(matches!(
            error,
            RuntimeError::Core(nerve_core::DispatchError::MissingToolName)
        ));
    }

    #[test]
    fn command_ping_returns_ok() {
        let response = runtime()
            .handle_command(RuntimeCommand::Ping)
            .expect("ping response");
        assert_eq!(response["status"], "ok");
    }

    #[test]
    fn command_tool_call_uses_runtime_dispatch() {
        let response = runtime()
            .handle_command(RuntimeCommand::ToolCall {
                name: "file_search".to_string(),
                arguments: arguments(json!({ "pattern": "alpha", "mode": "content" })),
            })
            .expect("tool response");
        assert_eq!(
            response["structuredContent"]["content_matches"][0]["path"],
            "notes.txt"
        );
    }

    #[test]
    fn command_tool_list_returns_specs() {
        let response = runtime()
            .handle_command(RuntimeCommand::ToolList)
            .expect("tool list");
        let names: Vec<_> = response["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"file_search"));
    }

    #[test]
    fn session_commands_are_advertised() {
        for command in [
            "session.start",
            "session.message",
            "session.interrupt",
            "session.respond",
            "session.get",
            "session.list",
            "session.close",
        ] {
            assert!(RUNTIME_COMMAND_NAMES.contains(&command));
        }
    }

    #[test]
    fn auth_commands_are_advertised() {
        for command in ["auth.start", "auth.complete", "auth.status", "auth.logout"] {
            assert!(RUNTIME_COMMAND_NAMES.contains(&command));
        }
    }

    #[test]
    fn session_commands_are_host_executed() {
        let error = runtime()
            .handle_command(RuntimeCommand::SessionList)
            .expect_err("session commands should be intercepted by the host");
        assert_eq!(error.kind(), "adapter");
        assert!(
            error
                .to_string()
                .contains("executed by the host session manager")
        );
    }

    #[test]
    fn auth_commands_are_host_executed() {
        let error = runtime()
            .handle_command(RuntimeCommand::AuthStatus {
                provider: "openai".to_string(),
            })
            .expect_err("auth commands should be intercepted by the host");
        assert_eq!(error.kind(), "adapter");
        assert!(
            error
                .to_string()
                .contains("executed by the host auth manager")
        );
    }

    #[test]
    fn session_command_serializes_protocol_shape() {
        let command = RuntimeCommand::SessionRespond {
            session_id: "session-1".to_string(),
            request_id: "request-1".to_string(),
            decision: SessionApprovalDecision::Allow,
        };
        let value = serde_json::to_value(command).expect("command json");
        assert_eq!(value["kind"], "session.respond");
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["request_id"], "request-1");
        assert_eq!(value["decision"], "allow");
    }

    #[test]
    fn auth_command_serializes_protocol_shape() {
        let command = RuntimeCommand::AuthComplete {
            login_id: "login-1".to_string(),
            code: Some("code-1".to_string()),
            callback_url: None,
        };
        let value = serde_json::to_value(command).expect("command json");
        assert_eq!(value["kind"], "auth.complete");
        assert_eq!(value["login_id"], "login-1");
        assert_eq!(value["code"], "code-1");
        assert!(value.get("callback_url").is_none());
    }

    #[test]
    fn session_events_serialize_protocol_shapes() {
        let lifecycle = RuntimeEvent::session_started("session-1");
        let lifecycle_value = serde_json::to_value(lifecycle).expect("lifecycle event json");
        assert_eq!(lifecycle_value["type"], "session_started");
        assert_eq!(lifecycle_value["session_id"], "session-1");

        let agent = RuntimeEvent::session_agent(
            "session-1",
            AgentEventKind::Message {
                text: "hello".to_string(),
            },
        );
        let agent_value = serde_json::to_value(agent).expect("session agent event json");
        assert_eq!(agent_value["type"], "session_agent");
        assert_eq!(agent_value["session_id"], "session-1");
        assert_eq!(agent_value["event"]["kind"], "message");
        assert_eq!(agent_value["event"]["text"], "hello");

        let approval = RuntimeEvent::approval_requested(
            "session-1",
            "request-1",
            "file_edit",
            json!({ "path": "README.md" }),
            RiskTier::Edit,
            "--- a/README.md\n+++ b/README.md",
        );
        let approval_value = serde_json::to_value(approval).expect("approval event json");
        assert_eq!(approval_value["type"], "approval_requested");
        assert_eq!(approval_value["session_id"], "session-1");
        assert_eq!(approval_value["request_id"], "request-1");
        assert_eq!(approval_value["tool"], "file_edit");
        assert_eq!(approval_value["arguments"]["path"], "README.md");
        assert_eq!(approval_value["tier"], "edit");
        assert_eq!(
            approval_value["preview"],
            "--- a/README.md\n+++ b/README.md"
        );
    }

    #[test]
    fn auth_events_serialize_protocol_shape() {
        let event = RuntimeEvent::auth("openai", AuthEventKind::LoginPending);
        let value = serde_json::to_value(event).expect("auth event json");
        assert_eq!(value["type"], "auth");
        assert_eq!(value["provider"], "openai");
        assert_eq!(value["kind"], "login_pending");
    }

    #[test]
    fn job_events_serialize_as_flat_payloads() {
        let event = RuntimeEvent::job_started(
            "job-1",
            &RuntimeCommand::ToolCall {
                name: "file_search".to_string(),
                arguments: arguments(json!({})),
            },
        );
        let value = serde_json::to_value(event).expect("event json");
        assert_eq!(value["type"], "job_started");
        assert_eq!(value["job_id"], "job-1");
        assert_eq!(value["command"], "tool.call");
        assert_eq!(value["tool_name"], "file_search");
    }

    #[test]
    fn job_snapshot_serializes_protocol_shape() {
        let snapshot = RuntimeJobSnapshot {
            job_id: "job-1".to_string(),
            status: RuntimeJobStatus::Completed,
            command: "tool.call".to_string(),
            tool_name: Some("file_search".to_string()),
            created_at_ms: 10,
            started_at_ms: Some(11),
            updated_at_ms: 20,
            finished_at_ms: Some(20),
            cancel_requested: false,
            result: Some(json!({ "ok": true })),
            error: None,
        };
        let value = serde_json::to_value(snapshot).expect("snapshot json");
        assert_eq!(value["job_id"], "job-1");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["command"], "tool.call");
        assert_eq!(value["tool_name"], "file_search");
        assert_eq!(value["result"]["ok"], true);
        assert!(value["error"].is_null());
    }

    #[test]
    fn job_request_defaults_deserialize() {
        let get: RuntimeJobGetRequest =
            serde_json::from_value(json!({ "job_id": "job-1" })).expect("get request");
        let list: RuntimeJobListRequest = serde_json::from_value(json!({})).expect("list request");
        assert!(get.include_result);
        assert!(list.include_terminal);
        assert!(!list.include_results);
        assert_eq!(list.limit, 100);
    }

    #[test]
    fn generated_protocol_rust_artifacts_are_current() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let schema = std::fs::read_to_string(root.join(protocol_codegen::SCHEMA_PATH))
            .expect("runtime protocol schema artifact");
        let constants = std::fs::read_to_string(root.join(protocol_codegen::CONSTANTS_PATH))
            .expect("runtime protocol constants artifact");
        assert_eq!(schema, protocol_codegen::schema_json());
        assert_eq!(constants, protocol_codegen::constants_json());
    }

    #[test]
    fn runtime_error_reports_cancelled_kind() {
        let error = RuntimeError::cancelled();
        assert_eq!(error.kind(), "cancelled");
        assert!(error.is_cancelled());
    }

    #[test]
    fn cancellable_command_stops_before_execution() {
        let cancel = nerve_core::CancelToken::new();
        cancel.cancel();
        let error = runtime()
            .handle_command_cancellable(RuntimeCommand::Ping, &cancel)
            .expect_err("cancelled command should fail");
        assert!(error.is_cancelled());
    }

    #[test]
    fn cancellable_adapter_default_stops_before_execution() {
        let cancel = nerve_core::CancelToken::new();
        cancel.cancel();
        let error = runtime()
            .with_adapter(AdapterTool)
            .handle_tool_call_cancellable(
                &json!({ "name": "adapter_tool", "arguments": {} }),
                &cancel,
            )
            .expect_err("cancelled adapter should fail");
        assert!(error.is_cancelled());
    }
}
