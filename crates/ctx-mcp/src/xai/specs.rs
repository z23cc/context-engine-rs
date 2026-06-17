use super::*;

pub(super) fn tool_specs() -> Vec<Value> {
    vec![
        spec_xai_models(),
        spec_xai_responses(),
        spec_xai_x_search(),
        spec_xai_web_search(),
        spec_xai_image_generate(),
        spec_xai_tts(),
        spec_xai_transcribe(),
        spec_xai_video_generate(),
    ]
}

pub(super) fn spec_xai_models() -> Value {
    json!({
        "name": "xai_models",
        "description": "List models from xAI using the stored xAI OAuth bearer.",
        "inputSchema": { "type": "object", "properties": {} }
    })
}

pub(super) fn spec_xai_responses() -> Value {
    json!({
        "name": "xai_responses",
        "description": "Call xAI /responses with arbitrary Responses API payload. Requires `ctx-mcp auth login xai` first.",
        "inputSchema": {
            "type": "object",
            "required": ["input"],
            "properties": {
                "model": { "type": "string", "default": DEFAULT_CHAT_MODEL },
                "input": { "description": "Responses API input: string or message array/object." },
                "tools": { "type": "array", "description": "Optional Responses API tools." },
                "instructions": { "type": "string" },
                "temperature": { "type": "number" },
                "max_output_tokens": { "type": "integer" },
                "reasoning": { "type": "object" },
                "timeout_seconds": { "type": "integer", "default": 180 }
            }
        }
    })
}

pub(super) fn spec_xai_x_search() -> Value {
    json!({
        "name": "xai_x_search",
        "description": "Search X/Twitter through xAI's built-in x_search Responses API tool using xAI OAuth.",
        "inputSchema": {
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "model": { "type": "string", "default": DEFAULT_X_SEARCH_MODEL },
                "allowed_x_handles": { "type": "array", "items": { "type": "string" }, "description": "Max 10 handles; mutually exclusive with excluded_x_handles." },
                "excluded_x_handles": { "type": "array", "items": { "type": "string" }, "description": "Max 10 handles; mutually exclusive with allowed_x_handles." },
                "from_date": { "type": "string", "description": "YYYY-MM-DD" },
                "to_date": { "type": "string", "description": "YYYY-MM-DD" },
                "enable_image_understanding": { "type": "boolean", "default": false },
                "enable_video_understanding": { "type": "boolean", "default": false },
                "timeout_seconds": { "type": "integer", "default": 180 }
            }
        }
    })
}

pub(super) fn spec_xai_web_search() -> Value {
    json!({
        "name": "xai_web_search",
        "description": "Search the web through xAI's web_search Responses API tool using xAI OAuth.",
        "inputSchema": {
            "type": "object",
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "default": 5, "minimum": 1, "maximum": 100 },
                "model": { "type": "string", "default": DEFAULT_WEB_SEARCH_MODEL },
                "allowed_domains": { "type": "array", "items": { "type": "string" }, "description": "Max 5; mutually exclusive with excluded_domains." },
                "excluded_domains": { "type": "array", "items": { "type": "string" }, "description": "Max 5; mutually exclusive with allowed_domains." },
                "timeout_seconds": { "type": "integer", "default": 90 }
            }
        }
    })
}

pub(super) fn spec_xai_image_generate() -> Value {
    json!({
        "name": "xai_image_generate",
        "description": "Generate an image with xAI Grok Imagine and save it to output_path.",
        "inputSchema": {
            "type": "object",
            "required": ["prompt", "output_path"],
            "properties": {
                "prompt": { "type": "string" },
                "workspace": { "type": "string", "description": "Workspace used to root-gate output_path. Defaults like core tools." },
                "output_path": { "type": "string", "description": "Destination inside the selected workspace root." },
                "model": { "type": "string", "default": DEFAULT_IMAGE_MODEL },
                "aspect_ratio": { "type": "string", "default": "1:1" },
                "resolution": { "type": "string", "default": "1k", "enum": ["1k", "2k"] },
                "download_url": { "type": "boolean", "default": true },
                "timeout_seconds": { "type": "integer", "default": 120 }
            }
        }
    })
}

pub(super) fn spec_xai_tts() -> Value {
    json!({
        "name": "xai_tts",
        "description": "Generate speech via xAI /tts and save audio to output_path.",
        "inputSchema": {
            "type": "object",
            "required": ["text", "output_path"],
            "properties": {
                "text": { "type": "string" },
                "workspace": { "type": "string", "description": "Workspace used to root-gate output_path. Defaults like core tools." },
                "output_path": { "type": "string", "description": "Destination inside the selected workspace root." },
                "voice_id": { "type": "string", "default": DEFAULT_TTS_VOICE },
                "language": { "type": "string", "default": DEFAULT_TTS_LANGUAGE },
                "output_format": { "type": "object", "description": "Optional xAI output_format, e.g. {codec,sample_rate,bit_rate}." },
                "timeout_seconds": { "type": "integer", "default": 60 }
            }
        }
    })
}

pub(super) fn spec_xai_transcribe() -> Value {
    json!({
        "name": "xai_transcribe",
        "description": "Transcribe an audio file via xAI /stt using multipart upload.",
        "inputSchema": {
            "type": "object",
            "required": ["file_path"],
            "properties": {
                "workspace": { "type": "string", "description": "Workspace used to root-gate file_path. Defaults like core tools." },
                "file_path": { "type": "string", "description": "Audio file inside the selected workspace root." },
                "language": { "type": "string" },
                "format": { "type": "boolean", "default": true },
                "diarize": { "type": "boolean", "default": false },
                "timeout_seconds": { "type": "integer", "default": 120 }
            }
        }
    })
}

pub(super) fn spec_xai_video_generate() -> Value {
    json!({
        "name": "xai_video_generate",
        "description": "Generate video with xAI Grok Imagine. Returns video_url and optionally downloads to output_path.",
        "inputSchema": {
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": { "type": "string" },
                "workspace": { "type": "string", "description": "Workspace used to root-gate output_path. Defaults like core tools." },
                "output_path": { "type": "string", "description": "Optional destination inside the selected workspace root." },
                "model": { "type": "string", "default": DEFAULT_VIDEO_MODEL },
                "image_url": { "type": "string", "description": "Optional image URL/data URI for image-to-video." },
                "reference_image_urls": { "type": "array", "items": { "type": "string" }, "description": "Optional reference images; max 7." },
                "duration": { "type": "integer", "default": 8, "minimum": 1, "maximum": 15 },
                "aspect_ratio": { "type": "string", "default": "16:9" },
                "resolution": { "type": "string", "default": "720p", "enum": ["480p", "720p"] },
                "timeout_seconds": { "type": "integer", "default": 240 },
                "poll_interval_seconds": { "type": "integer", "default": 5 }
            }
        }
    })
}
