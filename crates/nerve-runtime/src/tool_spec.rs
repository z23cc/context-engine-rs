//! Engine-coupled tool-spec helpers. The protocol `RuntimeToolSpec` type itself
//! lives in `nerve-proto` (wasm-safe, re-exported from the crate root); the
//! helpers below reach `nerve_core::tool_specs()` and so stay here.

use serde_json::Value;
use std::collections::HashSet;

pub(crate) fn core_tool_specs() -> Vec<Value> {
    nerve_core::tool_specs()
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn push_unique_tool_specs(
    tools: &mut Vec<Value>,
    names: &mut HashSet<String>,
    specs: Vec<Value>,
) {
    for spec in specs {
        let Some(name) = spec.get("name").and_then(Value::as_str) else {
            tools.push(spec);
            continue;
        };
        if names.insert(name.to_string()) {
            tools.push(spec);
        }
    }
}
