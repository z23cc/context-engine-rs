//! Formatting helpers for copyable tool traces.

pub(crate) fn tool_trace(tool: &str, status: &str, input: &str, output: &str) -> String {
    let mut sections = vec![format!("tool: {tool}\nstatus: {status}")];
    if !input.is_empty() {
        sections.push(format!("input:\n{input}"));
    }
    if !output.is_empty() {
        sections.push(format!("output:\n{output}"));
    }
    sections.join("\n\n")
}
