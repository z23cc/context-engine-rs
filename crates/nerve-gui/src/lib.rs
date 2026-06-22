//! Nerve Leptos CSR frontend (G1b spike) — library modules.
//!
//! A client-side-rendered (NOT SSR) single-page app: the `nerve daemon` is the
//! backend, reached **only** over HTTP `/rpc` (JSON-RPC Protocol v4) and
//! `/events` (SSE) — never Tauri IPC. It shares the engine's exact protocol
//! types via [`nerve_proto`] so there is no hand-duplicated vocabulary and no
//! TS/codegen drift.
//!
//! This wave proves the end-to-end pipeline (daemon serves the WASM bundle at
//! `/app`, the bundle reads the injected token, calls `runtime/info` +
//! `runtime/tools/list`, renders the result). The real chat surface is G2; the
//! final Codex styling is G4. The browser entry point lives in `main.rs`.

pub mod app;
pub(crate) mod approval;
pub(crate) mod context_view;
pub(crate) mod data;
pub(crate) mod dom;
pub(crate) mod events;
pub(crate) mod render;
pub mod rpc;
pub(crate) mod settings;
pub(crate) mod settings_auth;
pub(crate) mod sidebar;
pub(crate) mod topbar;
