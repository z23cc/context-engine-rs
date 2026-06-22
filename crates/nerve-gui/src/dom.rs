//! Tiny browser DOM helpers used by the GUI.

use wasm_bindgen::JsCast;

pub(crate) fn focus_message_input() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(document) = window.document() else {
        return;
    };
    let Some(element) = document.get_element_by_id("message") else {
        return;
    };
    if let Some(input) = element.dyn_ref::<web_sys::HtmlElement>() {
        let _ = input.focus();
    }
}
