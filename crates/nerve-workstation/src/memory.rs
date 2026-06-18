//! Project-local long-term memory tool and startup hook.
//!
//! This module is intentionally standalone until the agent/session wiring lands.

#![allow(dead_code)]

use nerve_agent::{
    AgentDef, AgentError, AgentResult, Hook, LlmProvider, Message, Orchestrator, ToolBox, ToolSpec,
};
use nerve_core::CancelToken;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) const LONG_TERM_MAX_CHARS: usize = 2000;
pub(crate) const DISTILL_MIN_TURNS: u32 = 4;
const REMEMBER: &str = "remember";
const MEMORY_HEADER: &str = "## Project memory (durable facts learned in past sessions)";

pub(crate) const DISTILLER_SYSTEM_PROMPT: &str = concat!(
    "Review the session above. Your ONLY job: if it produced a DURABLE, VERIFIED fact worth ",
    "recalling in FUTURE sessions — a user preference, a non-obvious project convention, or a ",
    "hard-won fix/gotcha — call `remember` once per such fact. Apply a HIGH bar: most sessions ",
    "produce nothing worth keeping. Do NOT record anything a tool can re-derive (file locations, ",
    "code structure, APIs), transient task state, unverified guesses, unexecuted plans, or ",
    "anything already in project memory (shown below). If nothing qualifies, do nothing and stop. ",
    "When unsure, do not record."
);

pub(crate) trait MemoryStore: Send + Sync {
    fn load(&self) -> Vec<String>;
    fn remember(&self, fact: &str) -> RememberOutcome;
    fn recall_block(&self) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RememberOutcome {
    Added,
    Duplicate,
    OverCap { current: Vec<String> },
}

pub(crate) struct FileMemoryStore {
    path: PathBuf,
}

impl FileMemoryStore {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn write_facts(&self, facts: &[String]) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, bullet_list(facts))
    }
}

impl MemoryStore for FileMemoryStore {
    fn load(&self) -> Vec<String> {
        load_facts(&self.path)
    }

    fn remember(&self, fact: &str) -> RememberOutcome {
        let fact = fact.trim();
        let mut facts = self.load();
        if fact.is_empty() || facts.iter().any(|existing| existing == fact) {
            return RememberOutcome::Duplicate;
        }

        let mut next = facts.clone();
        next.push(fact.to_string());
        if recall_block_for(&next).chars().count() > LONG_TERM_MAX_CHARS {
            return RememberOutcome::OverCap { current: facts };
        }

        facts.push(fact.to_string());
        let _ = self.write_facts(&facts);
        RememberOutcome::Added
    }

    fn recall_block(&self) -> Option<String> {
        let facts = self.load();
        if facts.is_empty() {
            None
        } else {
            Some(recall_block_for(&facts))
        }
    }
}

pub(crate) struct NoOpToolBox;

impl ToolBox for NoOpToolBox {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![]
    }

    fn call(&self, _name: &str, _args: &Value, _cancel: &CancelToken) -> AgentResult<Value> {
        Err(AgentError::Tool("no tools available".into()))
    }
}

pub(crate) struct MemoryToolBox<T: ToolBox> {
    inner: T,
    store: Arc<dyn MemoryStore>,
}

impl<T: ToolBox> MemoryToolBox<T> {
    pub(crate) fn new(inner: T, store: Arc<dyn MemoryStore>) -> Self {
        Self { inner, store }
    }
}

impl<T: ToolBox> ToolBox for MemoryToolBox<T> {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(remember_spec());
        specs
    }

    fn call(&self, name: &str, args: &Value, cancel: &CancelToken) -> AgentResult<Value> {
        if name != REMEMBER {
            return self.inner.call(name, args, cancel);
        }
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        let args: RememberArgs = serde_json::from_value(args.clone())
            .map_err(|err| AgentError::Tool(format!("invalid remember args: {err}")))?;
        match self.store.remember(&args.fact) {
            RememberOutcome::Added => Ok(json!({ "status": "added" })),
            RememberOutcome::Duplicate => Ok(json!({ "status": "duplicate" })),
            RememberOutcome::OverCap { current } => Ok(json!({
                "status": "over_cap",
                "current": current,
                "message": concat!(
                    "Long-term memory is at its cap. The current list is returned; ",
                    "prune or replace lower-value facts before trying again."
                )
            })),
        }
    }
}

pub(crate) struct MemoryHook {
    store: Arc<dyn MemoryStore>,
}

impl MemoryHook {
    pub(crate) fn new(store: Arc<dyn MemoryStore>) -> Self {
        Self { store }
    }
}

impl Hook for MemoryHook {
    fn on_start(&self, system_prompt: &mut String) {
        let Some(block) = self.store.recall_block() else {
            return;
        };
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&block);
    }
}

pub(crate) fn distill_session(
    provider: &dyn LlmProvider,
    store: &Arc<dyn MemoryStore>,
    model: &str,
    history: Vec<Message>,
    cancel: &CancelToken,
) {
    let mut system_prompt = DISTILLER_SYSTEM_PROMPT.to_string();
    system_prompt.push_str("\n\nAlready in project memory (do not re-record):\n");
    system_prompt.push_str(&bullet_list(&store.load()));

    let def = AgentDef {
        system_prompt,
        model: model.into(),
        max_turns: 2,
        ..AgentDef::default()
    };
    let toolbox = MemoryToolBox::new(NoOpToolBox, Arc::clone(store));
    let mut orchestrator = Orchestrator::new(provider, &toolbox, def).with_history(history);
    let _ = orchestrator.run(
        "Distill durable, verified session learnings into memory if any qualify.",
        cancel,
        &mut |_| {},
    );
}

#[derive(Deserialize)]
struct RememberArgs {
    fact: String,
}

fn remember_spec() -> ToolSpec {
    ToolSpec {
        name: REMEMBER.to_string(),
        description: concat!(
            "Record a **durable, verified** fact worth remembering in future sessions — a user ",
            "preference, a non-obvious project convention, or a hard-won fix/gotcha. Only record ",
            "what a **successful** action verified. Do **NOT** record: file locations or code ",
            "structure (a tool re-finds those exactly), transient task state (use ",
            "update_checkpoint), unverified guesses, unexecuted plans, or anything reconstructable ",
            "in a few tool calls. Keep each fact one tight line."
        )
        .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "fact": {
                    "type": "string",
                    "description": "One tight line: a durable, verified fact worth recalling in future sessions."
                }
            },
            "required": ["fact"],
            "additionalProperties": false
        }),
    }
}

fn load_facts(path: &Path) -> Vec<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(parse_bullet)
        .filter(|fact| !fact.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_bullet(line: &str) -> Option<&str> {
    line.trim().strip_prefix("- ").map(str::trim)
}

fn bullet_list(facts: &[String]) -> String {
    facts
        .iter()
        .map(|fact| format!("- {fact}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn recall_block_for(facts: &[String]) -> String {
    let mut block = MEMORY_HEADER.to_string();
    for fact in facts {
        block.push_str("\n- ");
        block.push_str(fact);
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_agent::{
        ChatDelta, ChatRequest, ChatResponse, FinishReason, ProviderId, ToolCall, Usage,
    };
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeInner {
        calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeInner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl ToolBox for FakeInner {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "read_file".into(),
                description: String::new(),
                input_schema: json!({ "type": "object" }),
            }]
        }

        fn call(&self, name: &str, args: &Value, _cancel: &CancelToken) -> AgentResult<Value> {
            self.calls
                .lock()
                .expect("calls lock")
                .push((name.to_string(), args.clone()));
            Ok(json!({ "name": name, "args": args }))
        }
    }

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

    struct FailingProvider;

    impl LlmProvider for FailingProvider {
        fn id(&self) -> ProviderId {
            ProviderId::Anthropic
        }

        fn chat(
            &self,
            _req: &ChatRequest,
            _cancel: &CancelToken,
            _sink: &mut dyn FnMut(ChatDelta),
        ) -> AgentResult<ChatResponse> {
            Err(AgentError::Provider("boom".into()))
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

    fn temp_store() -> (tempfile::TempDir, FileMemoryStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileMemoryStore::new(dir.path().join(".nerve/memory.md"));
        (dir, store)
    }

    fn shared_store(store: FileMemoryStore) -> Arc<dyn MemoryStore> {
        Arc::new(store)
    }

    #[test]
    fn remember_appends_fact_to_file() {
        let (_dir, store) = temp_store();

        assert_eq!(
            store.remember("  User prefers concise answers.  "),
            RememberOutcome::Added
        );

        assert_eq!(store.load(), vec!["User prefers concise answers."]);
    }

    #[test]
    fn fact_remembered_in_one_session_is_recalled_in_the_next() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".nerve/memory.md");

        // Session 1: write a durable fact through the `remember` tool.
        let store1 = shared_store(FileMemoryStore::new(path.clone()));
        let tools = MemoryToolBox::new(FakeInner::new(), Arc::clone(&store1));
        tools
            .call(
                "remember",
                &json!({ "fact": "Deploys run via Scripts/release.sh." }),
                &CancelToken::never(),
            )
            .expect("remember call");

        // Session 2: a fresh store + hook over the SAME path recalls it at on_start.
        let store2 = shared_store(FileMemoryStore::new(path));
        let mut system_prompt = String::from("base system");
        MemoryHook::new(store2).on_start(&mut system_prompt);

        assert!(system_prompt.contains("## Project memory"));
        assert!(system_prompt.contains("Deploys run via Scripts/release.sh."));
    }

    #[test]
    fn duplicate_fact_returns_duplicate_without_rewriting() {
        let (_dir, store) = temp_store();

        assert_eq!(
            store.remember("Use rtk for shell commands."),
            RememberOutcome::Added
        );
        assert_eq!(
            store.remember("Use rtk for shell commands."),
            RememberOutcome::Duplicate
        );

        assert_eq!(store.load(), vec!["Use rtk for shell commands."]);
    }

    #[test]
    fn over_cap_returns_current_without_writing() {
        let (_dir, store) = temp_store();
        let existing = "a".repeat(100);
        assert_eq!(store.remember(&existing), RememberOutcome::Added);
        let before = store.load();
        let oversized = "x".repeat(LONG_TERM_MAX_CHARS);

        let outcome = store.remember(&oversized);

        assert_eq!(
            outcome,
            RememberOutcome::OverCap {
                current: before.clone()
            }
        );
        assert_eq!(store.load(), before);
    }

    #[test]
    fn recall_block_formats_facts_and_empty_store_returns_none() {
        let (_dir, store) = temp_store();
        assert_eq!(store.recall_block(), None);

        assert_eq!(store.remember("Fact one."), RememberOutcome::Added);
        assert_eq!(store.remember("Fact two."), RememberOutcome::Added);

        assert_eq!(
            store.recall_block().as_deref(),
            Some(
                "## Project memory (durable facts learned in past sessions)\n- Fact one.\n- Fact two."
            )
        );
    }

    #[test]
    fn hook_appends_recall_block_when_present() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.remember("Durable project convention."),
            RememberOutcome::Added
        );
        let hook = MemoryHook::new(shared_store(store));
        let mut prompt = "base system".to_string();

        hook.on_start(&mut prompt);

        assert_eq!(
            prompt,
            "base system\n\n## Project memory (durable facts learned in past sessions)\n- Durable project convention."
        );
    }

    #[test]
    fn hook_appends_nothing_for_empty_store() {
        let (_dir, store) = temp_store();
        let hook = MemoryHook::new(shared_store(store));
        let mut prompt = "base system".to_string();

        hook.on_start(&mut prompt);

        assert_eq!(prompt, "base system");
    }

    #[test]
    fn distill_session_remembers_tool_call_fact() {
        let (_dir, store) = temp_store();
        let store = shared_store(store);
        let fact = "User wants release notes grouped by user-visible impact.";
        let provider = RecordingProvider::new(vec![
            response(
                "remembering",
                vec![tool_call("call_1", REMEMBER, json!({ "fact": fact }))],
                FinishReason::ToolUse,
            ),
            response("", Vec::new(), FinishReason::Stop),
        ]);

        distill_session(
            &provider,
            &store,
            "test-model",
            vec![Message::user("Please prepare release notes.")],
            &CancelToken::never(),
        );

        assert_eq!(store.load(), vec![fact]);
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].tools.len(), 1);
        assert_eq!(requests[0].tools[0].name, REMEMBER);
    }

    #[test]
    fn distill_session_stop_without_tool_calls_leaves_store_unchanged() {
        let (_dir, store) = temp_store();
        assert_eq!(
            store.remember("Existing durable fact."),
            RememberOutcome::Added
        );
        let store = shared_store(store);
        let before = store.load();
        let provider = RecordingProvider::new(vec![response(
            "nothing to keep",
            Vec::new(),
            FinishReason::Stop,
        )]);

        distill_session(
            &provider,
            &store,
            "test-model",
            vec![Message::user("Thanks")],
            &CancelToken::never(),
        );

        assert_eq!(store.load(), before);
        assert!(
            provider.requests()[0]
                .system
                .as_deref()
                .expect("system prompt")
                .contains("- Existing durable fact.")
        );
    }

    #[test]
    fn distill_session_swallows_provider_errors() {
        let (_dir, store) = temp_store();
        let store = shared_store(store);

        distill_session(
            &FailingProvider,
            &store,
            "test-model",
            vec![Message::user("hello")],
            &CancelToken::never(),
        );

        assert!(store.load().is_empty());
    }

    #[test]
    fn non_remember_calls_delegate_to_inner() {
        let (_dir, store) = temp_store();
        let tools = MemoryToolBox::new(FakeInner::new(), shared_store(store));

        let out = tools
            .call("read_file", &json!({ "path": "x" }), &CancelToken::never())
            .expect("delegated");

        assert_eq!(out["name"], "read_file");
        assert_eq!(out["args"], json!({ "path": "x" }));
    }
}
