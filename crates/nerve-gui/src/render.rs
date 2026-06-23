//! Transcript rendering helpers for the Leptos chat surface.

use crate::app::{Role, ToolCard, Turn};
use crate::clipboard::copy_text_with_note;
use crate::trace_format::tool_trace;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

struct RenderedMarkdown {
    html: String,
    code_blocks: Vec<String>,
}

struct CodeCapture {
    language: Option<String>,
    source: String,
}

pub(crate) fn render_turn(turn: Turn) -> AnyView {
    match turn.role {
        Role::User => {
            let text = turn.text;
            let copy_text_value = text.clone();
            let copy_note = RwSignal::new(String::new());
            view! {
                <div class="turn user" role="article" aria-label="User message">
                    <div class="turn-actions user-actions">
                        {turn_copy_button("Copy user message", copy_text_value, copy_note)}
                        {copy_status(copy_note)}
                    </div>
                    <div class="bubble" aria-label="User message text">{text}</div>
                </div>
            }
            .into_any()
        }
        Role::Assistant => {
            let response_text = turn.text.clone();
            let rendered = markdown_to_html(&turn.text);
            let html = rendered.html;
            let code_blocks = StoredValue::new(rendered.code_blocks);
            let reasoning = turn.reasoning.clone();
            let tools = turn.tools.clone();
            let streaming = turn.streaming;
            let copy_note = RwSignal::new(String::new());
            let assistant_label = if streaming {
                "Assistant response, streaming"
            } else {
                "Assistant response"
            };
            view! {
                <div class="turn assistant" role="article" aria-label=assistant_label aria-busy=if streaming { "true" } else { "false" }>
                    <div class="turn-actions assistant-actions">
                        {turn_copy_button("Copy assistant response", response_text, copy_note)}
                        {copy_status(copy_note)}
                    </div>
                    {(!reasoning.is_empty()).then(|| view! {
                        <details class="reasoning" aria-label="Assistant reasoning details">
                            <summary aria-label="Toggle assistant reasoning details">"Thought for this step"</summary>
                            <pre aria-label="Assistant reasoning text">{reasoning}</pre>
                        </details>
                    })}
                    <div class="md" inner_html=html on:click=move |ev| copy_code_block(ev, code_blocks, copy_note)></div>
                    {tools.into_iter().map(render_tool).collect_view()}
                    {streaming.then(|| view! { <span class="cursor" aria-hidden="true">"▋"</span> })}
                </div>
            }
            .into_any()
        }
    }
}

fn turn_copy_button(label: &'static str, text: String, note: RwSignal<String>) -> impl IntoView {
    view! {
        <button
            class="turn-copy"
            type="button"
            aria-label=label
            disabled=text.is_empty()
            on:click=move |_| {
                copy_text_with_note(text.clone(), note, "Copied message.");
            }
        >"Copy"</button>
    }
}

fn copy_status(note: RwSignal<String>) -> impl IntoView {
    view! {
        {move || (!note.get().is_empty()).then(|| view! {
            <span class="turn-copy-note" role="status">{note.get()}</span>
        })}
    }
}

fn copy_code_block(
    ev: leptos::ev::MouseEvent,
    code_blocks: StoredValue<Vec<String>>,
    note: RwSignal<String>,
) {
    let Some(button) = ev
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .and_then(|element| element.closest(".md-code-copy").ok().flatten())
    else {
        return;
    };
    let Some(index) = button
        .get_attribute("data-code-index")
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    ev.prevent_default();
    if let Some(code) = code_blocks.get_value().get(index).cloned() {
        copy_text_with_note(code, note, format!("Copied code block {}.", index + 1));
    }
}

fn render_tool(card: ToolCard) -> AnyView {
    let status = match card.ok {
        None => "run",
        Some(true) => "ok",
        Some(false) => "err",
    };
    let status_label = match card.ok {
        None => "running",
        Some(true) => "ok",
        Some(false) => "error",
    };
    let tool = card.tool;
    let input = card.input;
    let output = card.output;
    let has_trace = !input.is_empty() || !output.is_empty();
    let trace = if has_trace {
        tool_trace(&tool, status_label, &input, &output)
    } else {
        String::new()
    };
    let tool_label = format!("Tool call: {tool}, {status_label}");
    let trace_label = format!("Trace details for {tool}");
    let input_label = format!("Input trace for {tool}");
    let output_label = format!("Output trace for {tool}");
    let copy_note = RwSignal::new(String::new());
    view! {
        <div class=format!("tool {status}") role="group" aria-label=tool_label>
            <div class="tool-head">
                <span class="tool-name">{tool.clone()}</span>
                <div class="tool-head-actions">
                    {has_trace.then(|| view! {
                        <button
                            class="tool-copy"
                            type="button"
                            aria-label=format!("Copy trace for {tool}")
                            on:click={
                                let trace = trace.clone();
                                move |_| {
                                    copy_text_with_note(trace.clone(), copy_note, "Copied trace.");
                                }
                            }
                        >"Copy trace"</button>
                    })}
                    {move || (!copy_note.get().is_empty()).then(|| view! {
                        <span class="tool-copy-note" role="status">{copy_note.get()}</span>
                    })}
                    <span class=format!("tool-dot {status}") title=status_label aria-label=status_label></span>
                </div>
            </div>
            {has_trace.then(|| view! {
                <details class="tool-details" aria-label=trace_label>
                    <summary class="tool-summary" aria-label=format!("Toggle trace details for {tool}")>"trace"</summary>
                    {(!input.is_empty()).then(|| view! {
                        <div class="tool-detail-label">"input"</div>
                        <pre class="tool-out" aria-label=input_label>{input.clone()}</pre>
                    })}
                    {(!output.is_empty()).then(|| view! {
                        <div class="tool-detail-label">"output"</div>
                        <pre class="tool-out" aria-label=output_label>{output.clone()}</pre>
                    })}
                </details>
            })}
        </div>
    }
    .into_any()
}

fn markdown_to_html(src: &str) -> RenderedMarkdown {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut out = String::new();
    let mut events = Vec::new();
    let mut current_code: Option<CodeCapture> = None;
    let mut code_blocks = Vec::new();

    for event in Parser::new_ext(src, options) {
        if let Some(code) = current_code.as_mut() {
            match event {
                Event::End(TagEnd::CodeBlock) => {
                    let code = current_code.take().expect("code block is active");
                    push_code_block(&mut out, &mut code_blocks, code);
                }
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => {
                    code.source.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak => code.source.push('\n'),
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_markdown_events(&mut out, &mut events);
                current_code = Some(CodeCapture {
                    language: code_language(&kind),
                    source: String::new(),
                });
            }
            Event::Html(raw) | Event::InlineHtml(raw) => events.push(Event::Text(raw)),
            other => events.push(other),
        }
    }

    if let Some(code) = current_code.take() {
        push_code_block(&mut out, &mut code_blocks, code);
    }
    flush_markdown_events(&mut out, &mut events);
    RenderedMarkdown {
        html: out,
        code_blocks,
    }
}

fn flush_markdown_events<'a>(out: &mut String, events: &mut Vec<pulldown_cmark::Event<'a>>) {
    if events.is_empty() {
        return;
    }
    pulldown_cmark::html::push_html(out, events.drain(..));
}

fn push_code_block(out: &mut String, code_blocks: &mut Vec<String>, code: CodeCapture) {
    let index = code_blocks.len();
    code_blocks.push(code.source.clone());
    let label = code.language.as_deref().unwrap_or("code");
    let class = code
        .language
        .as_ref()
        .map(|lang| format!(" class=\"language-{lang}\""))
        .unwrap_or_default();
    out.push_str("<div class=\"md-code-block\">");
    out.push_str("<div class=\"md-code-head\">");
    out.push_str("<span>");
    escape_html(out, label);
    out.push_str("</span>");
    out.push_str(&format!(
        "<button type=\"button\" class=\"md-code-copy\" data-code-index=\"{index}\" aria-label=\"Copy code block {}\">Copy code</button>",
        index + 1
    ));
    out.push_str("</div><pre><code");
    out.push_str(&class);
    out.push('>');
    escape_html(out, &code.source);
    out.push_str("</code></pre></div>");
}

fn code_language(kind: &pulldown_cmark::CodeBlockKind<'_>) -> Option<String> {
    let pulldown_cmark::CodeBlockKind::Fenced(info) = kind else {
        return None;
    };
    let lang = info.split_whitespace().next()?.trim();
    if lang.is_empty() {
        return None;
    }
    let safe = lang
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '+' | '.' | '#'))
        .collect::<String>();
    (!safe.is_empty()).then_some(safe)
}

fn escape_html(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str(r"&amp;"),
            '<' => out.push_str(r"&lt;"),
            '>' => out.push_str(r"&gt;"),
            '"' => out.push_str(r"&quot;"),
            '\'' => out.push_str(r"&#39;"),
            _ => out.push(ch),
        }
    }
}
