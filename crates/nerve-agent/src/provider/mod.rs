//! LLM provider abstraction and the shared HTTP/SSE helpers.
//!
//! Each concrete provider (`anthropic`, `openai_responses`, `xai`) implements
//! [`LlmProvider`] over the shared, blocking [`http`] helpers. Tools available
//! to the model are exposed through a [`ToolBox`].

use nerve_core::CancelToken;

use crate::AgentResult;
use crate::auth::ProviderId;
use crate::message::{ChatDelta, ChatRequest, ChatResponse, ToolSpec};

pub mod anthropic;
pub mod http;
pub mod openai_responses;
mod retry;
pub mod xai;

/// A streaming chat-completion provider for one vendor.
pub trait LlmProvider: Send + Sync {
    /// The provider this implementation talks to.
    fn id(&self) -> ProviderId;

    /// Run a chat completion, streaming deltas into `sink` and returning the
    /// assembled response. Implementations must honor `cancel`.
    fn chat(
        &self,
        req: &ChatRequest,
        cancel: &CancelToken,
        sink: &mut dyn FnMut(ChatDelta),
    ) -> AgentResult<ChatResponse>;
}

/// A set of callable tools exposed to the model.
pub trait ToolBox: Send + Sync {
    /// The specs advertised to the model.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Invoke the tool `name` with `args`, honoring `cancel`.
    fn call(
        &self,
        name: &str,
        args: &serde_json::Value,
        cancel: &CancelToken,
    ) -> AgentResult<serde_json::Value>;
}
