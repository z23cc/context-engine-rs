//! Tiny browser DOM helpers used by the GUI.

use leptos::prelude::*;
use leptos::{ev, leptos_dom::helpers::window_event_listener};
use wasm_bindgen::JsCast;

const FOCUSABLE_SELECTOR: &str = concat!(
    "button:not([disabled]), ",
    "input:not([disabled]), ",
    "select:not([disabled]), ",
    "textarea:not([disabled]), ",
    "summary, ",
    "a[href], ",
    "[tabindex]:not([tabindex=\"-1\"])"
);
const CONTEXT_MENU_ALLOWED_SELECTOR: &str = concat!(
    "input, textarea, select, option, a[href], ",
    "[contenteditable]:not([contenteditable=\"false\"]), ",
    ".transcript, .md, .bubble, pre, code"
);

pub(crate) fn suppress_chrome_context_menu(target: Option<web_sys::EventTarget>) -> bool {
    let Some(element) = target.and_then(target_element) else {
        return false;
    };
    if matches_selector(&element, CONTEXT_MENU_ALLOWED_SELECTOR) {
        return false;
    }
    matches_selector(&element, "#nerve-shell, #nerve-shell *")
}

pub(crate) fn focus_message_input() {
    focus_element_by_id_next_frame("message");
}

pub(crate) fn focus_approval_allow() {
    focus_element_by_id_next_frame("approval-allow");
}

pub(crate) fn focus_command_search() {
    focus_element_by_id_next_frame("command-search");
}

pub(crate) fn focus_thread_search() {
    focus_element_by_id_next_frame("thread-search");
}

pub(crate) fn focus_thread_row(index: usize) {
    focus_element_by_id_next_frame_owned(format!("thread-row-{index}"));
}

pub(crate) fn focus_project_row(index: usize) {
    focus_element_by_id_next_frame_owned(format!("project-row-{index}"));
}

pub(crate) fn focus_projects_region() {
    if element_by_id("project-row-0").is_some() {
        focus_project_row(0);
    } else {
        focus_element_by_id_next_frame("project-add-button");
    }
}

pub(crate) fn focus_context_filter() {
    focus_element_by_id_next_frame("context-filter");
}

pub(crate) fn focus_context_row(index: usize) {
    focus_element_by_id_next_frame_owned(format!("context-row-{index}"));
}

pub(crate) fn focus_context_selected_row(index: usize) {
    focus_element_by_id_next_frame_owned(format!("context-selected-row-{index}"));
}

pub(crate) fn focus_model_agent_select() {
    focus_element_by_id_next_frame("model-agent-select");
}

pub(crate) fn close_model_picker() {
    close_model_picker_silent();
    focus_element_by_id_next_frame("model-picker-summary");
}

pub(crate) fn close_model_picker_silent() {
    if let Some(details) = element_by_id("model-menu") {
        let _ = details.remove_attribute("open");
    }
}

pub(crate) fn focus_settings_segment(group: &'static str, index: usize) {
    focus_element_by_id_next_frame_owned(format!("settings-{group}-{index}"));
}

pub(crate) fn focus_inspector_tab(tab: &'static str) {
    focus_element_by_id_next_frame_owned(format!("inspector-tab-{tab}"));
}

pub(crate) fn focus_review_file_row(tab: &'static str, index: usize) {
    focus_element_by_id_next_frame_owned(format!("review-file-row-{tab}-{index}"));
}

pub(crate) fn focus_tool_filter(filter: &'static str) {
    focus_element_by_id_next_frame_owned(format!("tool-filter-{filter}"));
}

pub(crate) fn focus_tool_row(index: usize) {
    focus_element_by_id_next_frame_owned(format!("tool-row-focus-{index}"));
}

pub(crate) fn focus_surface(mode: &str) {
    if mode == "context" {
        focus_context_filter();
    } else {
        focus_message_input();
    }
}

pub(crate) fn focus_next_workspace_region(
    mode: &str,
    inspector_open: bool,
    inspector_tab: &'static str,
    reverse: bool,
) {
    let regions = visible_workspace_regions(inspector_open);
    if regions.is_empty() {
        return;
    }
    let current = active_workspace_region().unwrap_or(WorkspaceRegion::Surface);
    let fallback = regions
        .iter()
        .position(|region| *region == WorkspaceRegion::Surface)
        .unwrap_or(0);
    let current_index = regions
        .iter()
        .position(|region| *region == current)
        .unwrap_or(fallback);
    let next_index = if reverse {
        current_index.checked_sub(1).unwrap_or(regions.len() - 1)
    } else {
        (current_index + 1) % regions.len()
    };
    focus_workspace_region(regions[next_index], mode, inspector_tab);
}

pub(crate) fn element_exists(id: &str) -> bool {
    element_by_id(id).is_some()
}

pub(crate) fn element_has_attribute(id: &str, attr: &str) -> bool {
    element_by_id(id).is_some_and(|element| element.has_attribute(attr))
}

pub(crate) fn focus_element_by_id_next_frame(id: &'static str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = wasm_bindgen::closure::Closure::once_into_js(move || focus_element_by_id(id));
    let _ = window.request_animation_frame(callback.unchecked_ref());
}

fn focus_element_by_id_next_frame_owned(id: String) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = wasm_bindgen::closure::Closure::once_into_js(move || focus_element_by_id(&id));
    let _ = window.request_animation_frame(callback.unchecked_ref());
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkspaceRegion {
    Projects,
    Threads,
    Surface,
    Inspector,
}

fn visible_workspace_regions(inspector_open: bool) -> Vec<WorkspaceRegion> {
    let mut regions = Vec::new();
    if element_is_visible("sidebar-panel") {
        regions.push(WorkspaceRegion::Projects);
        regions.push(WorkspaceRegion::Threads);
    }
    regions.push(WorkspaceRegion::Surface);
    if inspector_open && element_is_visible("inspector-panel") {
        regions.push(WorkspaceRegion::Inspector);
    }
    regions
}

fn active_workspace_region() -> Option<WorkspaceRegion> {
    let active = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.active_element())?;
    if matches_selector(&active, "#project-add-button, #project-add, .proj-list *") {
        return Some(WorkspaceRegion::Projects);
    }
    if matches_selector(&active, "#inspector-panel, #inspector-panel *") {
        return Some(WorkspaceRegion::Inspector);
    }
    if matches_selector(&active, ".main, .main *") {
        return Some(WorkspaceRegion::Surface);
    }
    if matches_selector(&active, ".sidebar, .sidebar *") {
        return Some(WorkspaceRegion::Threads);
    }
    None
}

fn focus_workspace_region(region: WorkspaceRegion, mode: &str, inspector_tab: &'static str) {
    match region {
        WorkspaceRegion::Projects => focus_projects_region(),
        WorkspaceRegion::Threads => focus_thread_search(),
        WorkspaceRegion::Surface => focus_surface(mode),
        WorkspaceRegion::Inspector => focus_inspector_tab(inspector_tab),
    }
}

fn matches_selector(element: &web_sys::Element, selector: &str) -> bool {
    element.closest(selector).ok().flatten().is_some()
}

fn target_element(target: web_sys::EventTarget) -> Option<web_sys::Element> {
    if let Ok(element) = target.clone().dyn_into::<web_sys::Element>() {
        return Some(element);
    }
    target
        .dyn_into::<web_sys::Node>()
        .ok()
        .and_then(|node| node.parent_element())
}

fn element_is_visible(id: &str) -> bool {
    element_by_id(id)
        .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
        .is_some_and(|element| element.offset_width() > 0 || element.offset_height() > 0)
}

/// Keep Tab traversal inside an active dialog. Returns true when it moved focus
/// and the caller should prevent the browser's default Tab handling.
pub(crate) fn trap_tab_focus(container_id: &str, shift: bool) -> bool {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return false;
    };
    let Some(container) = document.get_element_by_id(container_id) else {
        return false;
    };
    let Ok(nodes) = container.query_selector_all(FOCUSABLE_SELECTOR) else {
        return false;
    };
    let focusable = collect_focusable(&nodes);
    if focusable.is_empty() {
        focus_element_by_id(container_id);
        return true;
    }

    let active = document.active_element();
    let active_index = active.as_ref().and_then(|active| {
        focusable
            .iter()
            .position(|element| js_sys::Object::is(element.as_ref(), active.as_ref()))
    });
    let target = match (shift, active_index) {
        (true, Some(0) | None) => focusable.last(),
        (false, Some(index)) if index + 1 == focusable.len() => focusable.first(),
        (false, None) => focusable.first(),
        _ => return false,
    };
    if let Some(element) = target {
        let _ = element.focus();
        return true;
    }
    false
}

fn collect_focusable(nodes: &web_sys::NodeList) -> Vec<web_sys::HtmlElement> {
    let mut focusable = Vec::new();
    for index in 0..nodes.length() {
        if let Some(node) = nodes.item(index)
            && let Ok(element) = node.dyn_into::<web_sys::HtmlElement>()
        {
            focusable.push(element);
        }
    }
    focusable
}

fn focus_element_by_id(id: &str) {
    let Some(element) = element_by_id(id) else {
        return;
    };
    if let Some(input) = element.dyn_ref::<web_sys::HtmlElement>() {
        let _ = input.focus();
    }
}

fn element_by_id(id: &str) -> Option<web_sys::Element> {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
}

/// Install window-level chrome guards: suppress the native context menu outside
/// text fields, and F6 region navigation across workspace panes. Both listeners
/// self-remove on the current reactive owner's cleanup. Extracted from `App` to
/// keep that view fn under the file-size gate.
pub(crate) fn install_chrome_guards(
    mode: RwSignal<&'static str>,
    inspector_open: RwSignal<bool>,
    inspector_tab: RwSignal<&'static str>,
    settings_open: RwSignal<bool>,
    palette_open: RwSignal<bool>,
) {
    let chrome_context_menu = window_event_listener(ev::contextmenu, move |ev| {
        if suppress_chrome_context_menu(ev.target()) {
            ev.prevent_default();
        }
    });
    on_cleanup(move || chrome_context_menu.remove());

    let region_nav = window_event_listener(ev::keydown, move |ev| {
        if ev.default_prevented()
            || ev.key() != "F6"
            || ev.meta_key()
            || ev.ctrl_key()
            || ev.alt_key()
            || settings_open.get_untracked()
            || palette_open.get_untracked()
            || element_exists("approval-dialog")
            || element_has_attribute("model-menu", "open")
        {
            return;
        }
        ev.prevent_default();
        focus_next_workspace_region(
            mode.get_untracked(),
            inspector_open.get_untracked(),
            inspector_tab.get_untracked(),
            ev.shift_key(),
        );
    });
    on_cleanup(move || region_nav.remove());
}
