use crate::tool_spec::{core_tool_specs, push_unique_tool_specs};
use crate::{RuntimeCommand, RuntimeError, RuntimeToolAdapter};
use nerve_core::dispatch::DispatchProvider;
use nerve_core::{CancelToken, WorkspaceResolver, handle_tool_call_with_resolver_cancellable};
use serde_json::{Value, json};
use std::collections::HashSet;

/// Transport-neutral runtime shared by CLI, MCP, TUI, and future adapters.
pub struct Runtime<R>
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    resolver: R,
    adapters: Vec<Box<dyn RuntimeToolAdapter<R>>>,
}

impl<R> Runtime<R>
where
    R: WorkspaceResolver,
    R::Provider: DispatchProvider,
{
    #[must_use]
    pub fn new(resolver: R) -> Self {
        Self {
            resolver,
            adapters: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_adapter(mut self, adapter: impl RuntimeToolAdapter<R> + 'static) -> Self {
        self.add_adapter(adapter);
        self
    }

    pub fn add_adapter(&mut self, adapter: impl RuntimeToolAdapter<R> + 'static) {
        self.adapters.push(Box::new(adapter));
    }

    #[must_use]
    pub fn resolver(&self) -> &R {
        &self.resolver
    }

    #[must_use]
    pub fn tool_specs(&self) -> Value {
        let mut tools = Vec::new();
        let mut names = HashSet::new();
        push_unique_tool_specs(&mut tools, &mut names, core_tool_specs());
        for adapter in &self.adapters {
            push_unique_tool_specs(&mut tools, &mut names, adapter.tool_specs());
        }
        Value::Array(tools)
    }

    pub fn handle_tool_call(&self, params: &Value) -> Result<Value, RuntimeError> {
        let cancel = CancelToken::never();
        self.handle_tool_call_cancellable(params, &cancel)
    }

    pub fn handle_tool_call_cancellable(
        &self,
        params: &Value,
        cancel: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        for adapter in &self.adapters {
            if let Some(response) =
                adapter.handle_tool_call_cancellable(&self.resolver, params, cancel)?
            {
                return Ok(response);
            }
        }
        Ok(handle_tool_call_with_resolver_cancellable(
            &self.resolver,
            params,
            cancel,
        )?)
    }

    pub fn handle_command(&self, command: RuntimeCommand) -> Result<Value, RuntimeError> {
        let cancel = CancelToken::never();
        self.handle_command_cancellable(command, &cancel)
    }

    pub fn handle_command_cancellable(
        &self,
        command: RuntimeCommand,
        cancel: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::cancelled());
        }
        match command {
            RuntimeCommand::Ping => Ok(json!({ "status": "ok" })),
            RuntimeCommand::ToolList => Ok(json!({ "tools": self.tool_specs() })),
            RuntimeCommand::ToolCall { name, arguments } => self.handle_tool_call_cancellable(
                &json!({ "name": name, "arguments": arguments }),
                cancel,
            ),
            RuntimeCommand::AgentRun { .. } => Err(RuntimeError::adapter(
                "agent.run is executed by the host job manager, not the core runtime",
            )),
            RuntimeCommand::SessionStart { .. }
            | RuntimeCommand::SessionMessage { .. }
            | RuntimeCommand::SessionInterrupt { .. }
            | RuntimeCommand::SessionRespond { .. }
            | RuntimeCommand::SessionGet { .. }
            | RuntimeCommand::SessionList
            | RuntimeCommand::SessionClose { .. }
            | RuntimeCommand::SessionSetModel { .. }
            | RuntimeCommand::SessionSetMode { .. } => Err(RuntimeError::adapter(
                "session commands are executed by the host session manager, not the core runtime",
            )),
            RuntimeCommand::AuthStart { .. }
            | RuntimeCommand::AuthComplete { .. }
            | RuntimeCommand::AuthStatus { .. }
            | RuntimeCommand::AuthLogout { .. } => Err(RuntimeError::adapter(
                "auth commands are executed by the host auth manager, not the core runtime",
            )),
            RuntimeCommand::DelegateStart { .. }
            | RuntimeCommand::DelegateSteer { .. }
            | RuntimeCommand::DelegateClose { .. } => Err(RuntimeError::adapter(
                "delegate commands are executed by the host delegate runtime, not the core runtime",
            )),
            RuntimeCommand::FlowStart { .. }
            | RuntimeCommand::FlowSteer { .. }
            | RuntimeCommand::FlowReplay { .. }
            | RuntimeCommand::FlowGet { .. }
            | RuntimeCommand::FlowList
            | RuntimeCommand::FlowClose { .. }
            | RuntimeCommand::FlowRespond { .. } => Err(RuntimeError::adapter(
                "flow commands are executed by the host flow engine, not the core runtime",
            )),
        }
    }
}
