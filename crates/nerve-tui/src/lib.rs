//! `nerve-tui` — a Rust terminal UI that is a *client* of the Nerve runtime
//! protocol (Protocol v3 over stdio), never the engine.
//!
//! This crate speaks to `nerve daemon --stdio` exactly as the TypeScript client
//! does, reusing the `nerve-runtime` protocol types directly (no codegen). T1
//! ships the protocol client ([`protocol`]), a no-LLM smoke round-trip
//! ([`smoke`]), and a minimal streaming shell ([`app`]). Later waves port the
//! full rendering/commands/approval UX.

pub mod app;
pub mod protocol;
pub mod smoke;
