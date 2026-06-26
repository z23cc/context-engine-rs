use super::{http::*, media::*, util::*};
use crate::auth;
use anyhow::{Result, anyhow, bail};
use nerve_fs::FsWorkspaceRegistry;
use serde_json::{Value, json};
use std::time::Duration;

mod audio;
mod image;
mod models;
mod responses;
mod search;
mod video;

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
        "xai_models" => models::xai_models(&arguments),
        "xai_responses" => responses::xai_responses(&arguments),
        "x_search" | "xai_x_search" => search::xai_x_search(&arguments),
        "web_search" | "xai_web_search" => search::xai_web_search(&arguments),
        "xai_image_generate" => image::xai_image_generate(registry, &arguments),
        "xai_tts" => audio::xai_tts(registry, &arguments),
        "xai_transcribe" => audio::xai_transcribe(registry, &arguments),
        "xai_video_generate" => video::xai_video_generate(registry, &arguments),
        _ => return Ok(None),
    }?;
    Ok(Some(response))
}
