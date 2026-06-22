//! Transcript rendering helpers for the Leptos chat surface.

use crate::app::{Role, ToolCard, Turn};
use leptos::prelude::*;

pub(crate) fn render_turn(turn: Turn) -> AnyView {
    match turn.role {
        Role::User => view! {
            <div class="turn user"><div class="bubble">{turn.text}</div></div>
        }
        .into_any(),
        Role::Assistant => {
            let html = markdown_to_html(&turn.text);
            let reasoning = turn.reasoning.clone();
            let tools = turn.tools.clone();
            let streaming = turn.streaming;
            view! {
                <div class="turn assistant">
                    {(!reasoning.is_empty()).then(|| view! {
                        <details class="reasoning">
                            <summary>"Thought for this step"</summary>
                            <pre>{reasoning}</pre>
                        </details>
                    })}
                    <div class="md" inner_html=html></div>
                    {tools.into_iter().map(render_tool).collect_view()}
                    {streaming.then(|| view! { <span class="cursor">"▋"</span> })}
                </div>
            }
            .into_any()
        }
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
    view! {
        <div class=format!("tool {status}")>
            <div class="tool-head">
                <span class="tool-name">{card.tool}</span>
                <span class=format!("tool-dot {status}") title=status_label aria-label=status_label></span>
            </div>
            {(!card.output.is_empty()).then(|| view! {
                <details class="tool-details">
                    <summary class="tool-summary">"output"</summary>
                    <pre class="tool-out">{card.output}</pre>
                </details>
            })}
        </div>
    }
    .into_any()
}

fn markdown_to_html(src: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(src, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}
