//! `nerve-wechat` — a personal-WeChat (个人微信) **client surface** for nerve.
//!
//! This is a runtime-protocol client (a sibling of `nerve-tui`), NOT engine code:
//! it lives entirely outside `nerve-core`'s determinism boundary. It bridges two
//! protocols so you can drive your local `nerve daemon` — and through it external
//! CLI coding agents (`delegate.*`) — from a WeChat chat:
//!
//! - **WeChat side:** Tencent's *official* iLink Bot gateway (HTTP long-poll, QR
//!   login → bearer token). This is a sanctioned bot transport, not a
//!   reverse-engineered client hook — see [`gateway`] / [`login`].
//! - **nerve side:** the [`bridge`] maps an inbound message to a `delegate.start` /
//!   `delegate.steer` (read-only by default) via the [`bridge::NerveControl`] seam
//!   and streams the reply back.
//!
//! Account safety is structural: a [`bridge::SenderAllowlist`] (fail-closed) means
//! only WeChat ids you list can command the agent.
//!
//! ## Status
//! This slice ships the pure-Rust iLink client (types, login, gateway) and the
//! safety/mapping bridge core, all unit-tested. The real `NerveControl` over the
//! daemon runtime protocol (`/rpc` + SSE) and the `nerve wechat` binary (live QR
//! login + run loop) are the next slice.

pub mod bridge;
pub mod config;
pub mod error;
pub mod gateway;
pub mod http;
pub mod login;
pub mod nerve_client;
pub mod session_store;
pub mod types;

pub use bridge::{Bridge, BridgeError, NerveControl, NerveReply, SenderAllowlist, chat_key};
pub use config::WechatConfig;
pub use error::{WeixinError, WeixinResult};
pub use gateway::{CDN_BASE_URL, DEFAULT_BASE_URL, IlinkGateway, WeixinGateway};
pub use login::{QrStart, QrStatus, WeixinSession, poll_qr_once, qr_login, start_qr_login};
pub use nerve_client::DelegateNerve;
pub use session_store::{load_session, save_session};
pub use types::{GetUpdatesResp, MessageItem, WeixinMessage};
