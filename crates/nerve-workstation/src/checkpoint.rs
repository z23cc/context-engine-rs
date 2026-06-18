//! Working-memory checkpoint tool and request hook.
//!
//! This module is intentionally standalone until the agent/session wiring lands.

#![allow(dead_code)]

use nerve_agent::{AgentError, AgentResult, ChatRequest, Hook, ToolBox, ToolSpec};
use nerve_core::CancelToken;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub(crate) const CHECKPOINT_MAX_CHARS: usize = 1500;
const UPDATE_CHECKPOINT: &str = "update_checkpoint";
const TRUNCATED_MARKER: &str = " …[truncated]";
const WORKING_MEMORY_HEADER: &str = "\n\n## Working memory (your running notes)\n";

type SharedCheckpoint = Arc<Mutex<Checkpoint>>;

pub(crate) struct Checkpoint {
    pub(crate) note: String,
}

impl Checkpoint {
    pub(crate) fn new() -> Self {
        Self {
            note: String::new(),
        }
    }

    pub(crate) fn replace(&mut self, note: impl Into<String>) -> bool {
        let (note, truncated) = cap_note(note.into());
        self.note = note;
        truncated
    }
}

pub(crate) struct CheckpointToolBox<T: ToolBox> {
    inner: T,
    checkpoint: SharedCheckpoint,
}

impl<T: ToolBox> CheckpointToolBox<T> {
    pub(crate) fn new(inner: T, checkpoint: SharedCheckpoint) -> Self {
        Self { inner, checkpoint }
    }
}

impl<T: ToolBox> ToolBox for CheckpointToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(update_checkpoint_spec());
        specs
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if name != UPDATE_CHECKPOINT {
            return self.inner.call(name, args, cancel);
        }
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        let args: UpdateCheckpointArgs = serde_json::from_value(args.clone())
            .map_err(|err| AgentError::Tool(format!("invalid update_checkpoint args: {err}")))?;
        let mut checkpoint = lock_checkpoint(&self.checkpoint);
        let truncated = checkpoint.replace(args.note);
        Ok(json!({
            "status": "ok",
            "chars": checkpoint.note.chars().count(),
            "truncated": truncated,
        }))
    }
}

pub(crate) struct CheckpointHook {
    checkpoint: SharedCheckpoint,
}

impl CheckpointHook {
    pub(crate) fn new(checkpoint: SharedCheckpoint) -> Self {
        Self { checkpoint }
    }
}

impl Hook for CheckpointHook {
    fn on_request(&self, req: &mut ChatRequest) {
        let note = lock_checkpoint(&self.checkpoint).note.clone();
        if note.is_empty() {
            return;
        }

        let system = req.system.get_or_insert_with(String::new);
        system.push_str(WORKING_MEMORY_HEADER);
        system.push_str(&note);
    }
}

#[derive(Deserialize)]
struct UpdateCheckpointArgs {
    note: String,
}

fn update_checkpoint_spec() -> ToolSpec {
    ToolSpec {
        name: UPDATE_CHECKPOINT.to_string(),
        description: concat!(
            "Replace your working-memory checkpoint note. Store only durable plan, ",
            "decisions, progress, next steps, and pointers like `path:line`; do NOT ",
            "store file contents, raw tool output, guesses, unexecuted plans, ",
            "volatile state, or anything reconstructable in a few tool calls."
        )
        .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "note": {
                    "type": "string",
                    "description": concat!(
                        "Replacement checkpoint note. Keep it concise: durable plan, ",
                        "decisions, progress, next steps, and `path:line` pointers only."
                    )
                }
            },
            "required": ["note"],
            "additionalProperties": false
        }),
    }
}

fn cap_note(note: String) -> (String, bool) {
    if note.chars().count() <= CHECKPOINT_MAX_CHARS {
        return (note, false);
    }

    let keep = CHECKPOINT_MAX_CHARS - TRUNCATED_MARKER.chars().count();
    let mut capped: String = note.chars().take(keep).collect();
    capped.push_str(TRUNCATED_MARKER);
    (capped, true)
}

fn lock_checkpoint(checkpoint: &SharedCheckpoint) -> std::sync::MutexGuard<'_, Checkpoint> {
    match checkpoint.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_agent::{
        AgentDef, ChatDelta, ChatResponse, FinishReason, LlmProvider, Message, Orchestrator,
        ProviderId, ToolBox, ToolCall, Usage,
    };
    use std::collections::VecDeque;

    struct RecordingProvider {
        responses: Mutex<VecDeque<ChatResponse>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl RecordingProvider {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().expect("requests lock").clone()
        }
    }

    impl LlmProvider for RecordingProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Anthropic
        }

        fn chat(
            &self,
            req: &ChatRequest,
            _cancel: &CancelToken,
            sink: &mut dyn FnMut(ChatDelta),
        ) -> AgentResult<ChatResponse> {
            self.requests
                .lock()
                .expect("requests lock")
                .push(req.clone());
            let response = self
                .responses
                .lock()
                .expect("responses lock")
                .pop_front()
                .expect("scripted response");
            if !response.content.is_empty() {
                sink(ChatDelta::Text(response.content.clone()));
            }
            Ok(response)
        }
    }

    struct FakeInner;

    impl ToolBox for FakeInner {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "read_file".into(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
            }]
        }

        fn call(&self, name: &str, args: &Value, _cancel: &CancelToken) -> AgentResult<Value> {
            Ok(json!({ "name": name, "args": args }))
        }
    }

    fn shared() -> SharedCheckpoint {
        Arc::new(Mutex::new(Checkpoint::new()))
    }

    fn tools(checkpoint: SharedCheckpoint) -> CheckpointToolBox<FakeInner> {
        CheckpointToolBox::new(FakeInner, checkpoint)
    }

    fn request(system: Option<&str>) -> ChatRequest {
        ChatRequest {
            model: "test-model".into(),
            system: system.map(str::to_string),
            messages: vec![Message::user("hi")],
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
        }
    }

    fn tool_call(id: impl Into<String>, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }

    fn response(
        content: &str,
        tool_calls: Vec<ToolCall>,
        finish_reason: FinishReason,
    ) -> ChatResponse {
        ChatResponse {
            content: content.into(),
            reasoning: None,
            tool_calls,
            finish_reason,
            usage: Usage::default(),
        }
    }

    #[test]
    fn update_checkpoint_replaces_existing_note() {
        let checkpoint = shared();
        let tools = tools(Arc::clone(&checkpoint));

        tools
            .call(
                UPDATE_CHECKPOINT,
                &json!({ "note": "first" }),
                &CancelToken::never(),
            )
            .expect("first update succeeds");
        tools
            .call(
                UPDATE_CHECKPOINT,
                &json!({ "note": "second" }),
                &CancelToken::never(),
            )
            .expect("second update succeeds");

        assert_eq!(lock_checkpoint(&checkpoint).note, "second");
    }

    #[test]
    fn over_cap_update_truncates_with_marker() {
        let checkpoint = shared();
        let tools = tools(Arc::clone(&checkpoint));
        let long_note = "x".repeat(CHECKPOINT_MAX_CHARS + 20);

        let out = tools
            .call(
                UPDATE_CHECKPOINT,
                &json!({ "note": long_note }),
                &CancelToken::never(),
            )
            .expect("update succeeds");
        let note = lock_checkpoint(&checkpoint).note.clone();

        assert_eq!(out["truncated"], true);
        assert_eq!(note.chars().count(), CHECKPOINT_MAX_CHARS);
        assert!(note.ends_with(TRUNCATED_MARKER));
    }

    #[test]
    fn hook_injects_non_empty_note_into_system() {
        let checkpoint = shared();
        lock_checkpoint(&checkpoint).replace("plan: continue at src/main.rs:1");
        let hook = CheckpointHook::new(checkpoint);
        let mut req = request(Some("base system"));

        hook.on_request(&mut req);

        assert_eq!(
            req.system.as_deref(),
            Some(
                "base system\n\n## Working memory (your running notes)\nplan: continue at src/main.rs:1"
            )
        );
    }

    #[test]
    fn hook_creates_system_when_missing() {
        let checkpoint = shared();
        lock_checkpoint(&checkpoint).replace("next: run tests");
        let hook = CheckpointHook::new(checkpoint);
        let mut req = request(None);

        hook.on_request(&mut req);

        assert_eq!(
            req.system.as_deref(),
            Some("\n\n## Working memory (your running notes)\nnext: run tests")
        );
    }

    #[test]
    fn empty_note_injects_nothing() {
        let checkpoint = shared();
        let hook = CheckpointHook::new(checkpoint);
        let mut req = request(Some("base system"));

        hook.on_request(&mut req);

        assert_eq!(req.system.as_deref(), Some("base system"));
    }

    #[test]
    fn checkpoint_hook_keeps_note_in_requests_after_history_compaction() {
        let checkpoint = shared();
        let note = "pin: continue from crates/nerve-workstation/src/agent.rs:334";
        let mut calls = vec![tool_call(
            "checkpoint",
            UPDATE_CHECKPOINT,
            json!({ "note": note }),
        )];
        let large_payload = "x".repeat(30_000);
        for idx in 0..12 {
            calls.push(tool_call(
                format!("read_{idx}"),
                "read_file",
                json!({ "path": format!("file_{idx}.rs"), "content": large_payload }),
            ));
        }
        let provider = RecordingProvider::new(vec![
            response("write note and grow history", calls, FinishReason::ToolUse),
            response("done", Vec::new(), FinishReason::Stop),
        ]);
        let tools = tools(Arc::clone(&checkpoint));
        let hook = CheckpointHook::new(checkpoint);
        let def = AgentDef {
            system_prompt: "base system".into(),
            model: "mock-model".into(),
            max_turns: 2,
            ..AgentDef::default()
        };
        let mut orchestrator = Orchestrator::new(&provider, &tools, def).with_hooks(vec![&hook]);

        orchestrator
            .run("go", &CancelToken::never(), &mut |_| {})
            .expect("run completes");

        let requests = provider.requests();
        let latest_system = requests
            .last()
            .and_then(|request| request.system.as_deref())
            .expect("latest request has system prompt");
        assert_eq!(requests.len(), 2);
        assert!(
            orchestrator
                .history()
                .iter()
                .any(|message| message.content == "[tool output elided to fit context]")
        );
        assert!(latest_system.contains("## Working memory (your running notes)"));
        assert!(latest_system.contains(note));
    }

    #[test]
    fn non_checkpoint_calls_delegate_to_inner() {
        let out = tools(shared())
            .call("read_file", &json!({ "path": "x" }), &CancelToken::never())
            .expect("delegated");

        assert_eq!(out["name"], "read_file");
    }
}
