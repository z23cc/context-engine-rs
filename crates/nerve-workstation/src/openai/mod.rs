use anyhow::Result;
use nerve_fs::FsWorkspaceRegistry;
use serde_json::Value;

const DEFAULT_CODEX_CHAT_MODEL: &str = "gpt-5.5";
const API_IMAGE_MODEL: &str = "gpt-image-2";

mod catalog;
mod http;
mod media;
mod specs;
mod tools;
mod util;

#[must_use]
pub(crate) fn tool_specs() -> Vec<Value> {
    specs::tool_specs()
}

pub(crate) fn handle_tool_call(
    registry: &FsWorkspaceRegistry,
    params: &Value,
) -> Result<Option<Value>> {
    tools::handle_tool_call(registry, params)
}

/// If `provider` is OpenAI/ChatGPT and `error` reads like a model-not-found or
/// unsupported-model failure, return a hint listing curated Codex ids.
pub(crate) fn model_error_hint(provider: &str, error: &str) -> Option<String> {
    let provider = provider.to_ascii_lowercase();
    if provider != "openai" && provider != "chatgpt" {
        return None;
    }
    let error = error.to_ascii_lowercase();
    let model_error = error.contains("does not exist")
        || error.contains("does not have access")
        || (error.contains("model")
            && (error.contains("not support") || error.contains("not found")));
    model_error.then(|| {
        format!(
            "known OpenAI Codex models: {}",
            catalog::CURATED_MODELS.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn lists_openai_tools() {
        let tools = tool_specs();
        let names: Vec<_> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"openai_image_generate"));
        assert!(names.contains(&"openai_models"));
    }

    #[test]
    fn unknown_tool_is_not_claimed() {
        let registry = FsWorkspaceRegistry::new();
        let result = handle_tool_call(
            &registry,
            &json!({ "name": "file_search", "arguments": {} }),
        )
        .expect("dispatch");
        assert!(result.is_none());
    }

    #[test]
    fn model_error_hint_targets_openai_model_errors() {
        assert!(
            model_error_hint("chatgpt", "HTTP 404: model foo does not exist")
                .expect("hint")
                .contains("gpt-5.5")
        );
        assert!(model_error_hint("openai", "model X is not supported").is_some());
        assert!(model_error_hint("xai", "model foo does not exist").is_none());
        assert!(model_error_hint("openai", "network timeout").is_none());
    }
}
