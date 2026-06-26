use super::{http::*, media::*, util::*};
use crate::auth;
use anyhow::Result;
use nerve_fs::FsWorkspaceRegistry;
use serde_json::{Value, json};
use std::time::Duration;

mod image;
mod models;

pub(super) fn handle_tool_call(
    registry: &FsWorkspaceRegistry,
    params: &Value,
) -> Result<Option<Value>> {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Ok(None);
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let response = match name {
        "openai_image_generate" => image::openai_image_generate(registry, &arguments),
        "openai_models" => models::openai_models(&arguments),
        _ => return Ok(None),
    }?;
    Ok(Some(response))
}
