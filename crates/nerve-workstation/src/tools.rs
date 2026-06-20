use crate::{delegate, openai, xai};
use nerve_core::{WorkspaceRegistry, WorkspaceResolver};
use nerve_runtime::{Runtime, RuntimeError, RuntimeToolAdapter};
use serde_json::Value;

pub(crate) type NerveRuntime = Runtime<WorkspaceRegistry>;

struct XaiToolAdapter;
struct OpenAiToolAdapter;

/// Read-only discovery adapter for the external-agent delegation feature. Owns
/// the `list_agents` tool; the actual `delegate.start` job runs through the job
/// manager's delegate executor, not this adapter.
struct DelegateToolAdapter;

impl RuntimeToolAdapter<WorkspaceRegistry> for XaiToolAdapter {
    fn tool_specs(&self) -> Vec<Value> {
        xai::tool_specs()
    }

    fn handle_tool_call(
        &self,
        registry: &WorkspaceRegistry,
        params: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        xai::handle_tool_call(registry, params)
            .map_err(|err| RuntimeError::adapter(err.to_string()))
    }
}

impl RuntimeToolAdapter<WorkspaceRegistry> for OpenAiToolAdapter {
    fn tool_specs(&self) -> Vec<Value> {
        openai::tool_specs()
    }

    fn handle_tool_call(
        &self,
        registry: &WorkspaceRegistry,
        params: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        openai::handle_tool_call(registry, params)
            .map_err(|err| RuntimeError::adapter(err.to_string()))
    }
}

impl RuntimeToolAdapter<WorkspaceRegistry> for DelegateToolAdapter {
    fn tool_specs(&self) -> Vec<Value> {
        delegate::tool_specs()
    }

    fn handle_tool_call(
        &self,
        registry: &WorkspaceRegistry,
        params: &Value,
    ) -> Result<Option<Value>, RuntimeError> {
        // Resolve the workspace root so `list_agents` discovers the C6 worker-as-data
        // catalog under `.nerve/workers` (built-ins still resolve when no root does).
        let root = registry
            .resolve_workspace(None)
            .ok()
            .and_then(|ws| ws.roots().first().map(|r| r.path.clone()));
        Ok(delegate::handle_tool_call(params, root.as_deref()))
    }
}

pub(crate) fn runtime(registry: WorkspaceRegistry) -> NerveRuntime {
    Runtime::new(registry)
        .with_adapter(XaiToolAdapter)
        .with_adapter(OpenAiToolAdapter)
        .with_adapter(DelegateToolAdapter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::{FsCatalogProvider, WorkspaceRegistry};
    use std::collections::HashSet;

    #[test]
    fn runtime_tool_specs_include_core_and_xai_tools() {
        let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
        let runtime = runtime(registry);
        let specs = runtime.tool_specs();
        let names: Vec<_> = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        let mut seen = HashSet::new();
        for name in &names {
            assert!(seen.insert(*name), "duplicate tool spec: {name}");
        }
        assert!(names.contains(&"file_search"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"x_search"));
        assert!(names.contains(&"xai_responses"));
        assert!(names.contains(&"xai_x_search"));
        assert!(names.contains(&"openai_image_generate"));
        assert!(names.contains(&"openai_models"));
        assert!(names.contains(&"list_agents"));
    }

    #[test]
    fn runtime_dispatches_list_agents() {
        let registry: WorkspaceRegistry<FsCatalogProvider> = WorkspaceRegistry::new();
        let runtime = runtime(registry);
        let response = runtime
            .handle_tool_call(&serde_json::json!({ "name": "list_agents", "arguments": {} }))
            .expect("list_agents response");
        let agents = response["agents"].as_array().expect("agents array");
        assert!(agents.iter().any(|a| a["name"] == "codex"));
    }
}
