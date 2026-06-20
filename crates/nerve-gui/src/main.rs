//! Trunk's CSR entry point for the Nerve Leptos frontend.
//!
//! Installs the panic hook (so a WASM panic prints a readable message to the
//! browser console instead of an opaque `unreachable`) and mounts the Leptos
//! app into `<body>`. The app modules live in the sibling `nerve_gui` lib.

use nerve_gui::app::App;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
