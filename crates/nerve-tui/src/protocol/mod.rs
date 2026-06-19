//! Runtime-protocol client (Protocol v3 over JSON-RPC 2.0 / NDJSON).
//!
//! The TUI is a *client* of the versioned runtime protocol, never the engine:
//! this module spawns `nerve daemon --stdio` and speaks to it over stdio,
//! exactly like `packages/tui/src/backend/nerveClient.ts`. The protocol
//! vocabulary (`RuntimeCommand`/`RuntimeEvent`/`RuntimeInfo`/job types) is reused
//! directly from `nerve-runtime`; only the JSON-RPC envelope is hand-rolled.

mod client;
mod envelope;
mod handshake;
mod spawn;

pub use client::NerveClient;
pub use handshake::validate_runtime_info;
pub use spawn::DaemonSpec;
