use super::*;

#[test]
fn symbolic_edit_tools_are_listed() {
    let specs = tool_specs();
    for name in [
        "replace_symbol_body",
        "insert_before_symbol",
        "insert_after_symbol",
    ] {
        let tool = specs
            .as_array()
            .expect("tool specs array")
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            .unwrap_or_else(|| panic!("{name} spec"));
        let required = tool["inputSchema"]["required"]
            .as_array()
            .expect("required");
        assert!(required.contains(&Value::String("symbol".to_string())));
        assert!(required.contains(&Value::String("body".to_string())));
    }
}

#[test]
fn rename_symbol_is_listed() {
    let specs = tool_specs();
    let tool = specs
        .as_array()
        .expect("tool specs array")
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some("rename_symbol"))
        .expect("rename_symbol spec");
    let required = tool["inputSchema"]["required"]
        .as_array()
        .expect("required");
    assert!(required.contains(&Value::String("symbol".to_string())));
    assert!(required.contains(&Value::String("new_name".to_string())));
}
