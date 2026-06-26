use anyhow::Result;
use nerve_fs::FsWorkspaceRegistry;
use serde_json::Value;

const DEFAULT_CHAT_MODEL: &str = "grok-build-0.1";
const DEFAULT_X_SEARCH_MODEL: &str = "grok-4.20-0309-reasoning";
const DEFAULT_WEB_SEARCH_MODEL: &str = "grok-build-0.1";
const DEFAULT_IMAGE_MODEL: &str = "grok-imagine-image";
const DEFAULT_VIDEO_MODEL: &str = "grok-imagine-video";
const DEFAULT_IMAGE_TO_VIDEO_MODEL: &str = "grok-imagine-video-1.5";
const DEFAULT_TTS_VOICE: &str = "eve";
const DEFAULT_TTS_LANGUAGE: &str = "en";

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

/// If `provider` is xAI and `error` reads like a model-not-found/unsupported
/// failure, return a hint listing the curated model ids (xAI's `/v1/models`
/// under-reports, so we keep a curated list); otherwise `None`.
pub(crate) fn model_error_hint(provider: &str, error: &str) -> Option<String> {
    let provider = provider.to_ascii_lowercase();
    if provider != "xai" && provider != "grok" {
        return None;
    }
    let error = error.to_ascii_lowercase();
    let model_error = error.contains("does not exist")
        || error.contains("does not have access")
        || (error.contains("model")
            && (error.contains("not support") || error.contains("not found")));
    model_error.then(|| {
        let ids: Vec<&str> = catalog::CURATED_MODELS.iter().map(|(id, _)| *id).collect();
        format!("known xAI models: {}", ids.join(", "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn lists_xai_tools() {
        let tools = tool_specs();
        let names: Vec<_> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"xai_responses"));
        assert!(names.contains(&"x_search"));
        assert!(names.contains(&"xai_x_search"));
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"xai_web_search"));
        assert!(names.contains(&"xai_image_generate"));
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
    fn model_error_hint_targets_xai_model_errors() {
        assert!(
            model_error_hint("xai", "HTTP 404: model foo does not exist")
                .expect("hint")
                .contains("grok-composer-2.5-fast")
        );
        assert!(model_error_hint("grok", "model X is not supported").is_some());
        assert!(model_error_hint("claude", "model foo does not exist").is_none());
        assert!(model_error_hint("xai", "network timeout").is_none());
    }
}
