//! QR-code login against the iLink Bot gateway.
//!
//! Flow (from `openclaw-weixin` `src/auth/login-qr.ts`): POST
//! `ilink/bot/get_bot_qrcode?bot_type=<T>` → `{ qrcode, qrcode_img_content }`;
//! then poll GET `ilink/bot/get_qrcode_status?qrcode=<id>` every ~1s until the
//! status is terminal. On `confirmed` the gateway returns the durable
//! `bot_token`, the per-account API `baseurl`, and the account/user ids.
//!
//! NOTE: `bot_type` (`DEFAULT_ILINK_BOT_TYPE` in the plugin) is not published in
//! the source we could read, so it is a required caller-supplied parameter rather
//! than a guessed constant.

use crate::error::{WeixinError, WeixinResult};
use crate::http;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// QR-login poll status (`get_qrcode_status` `status` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QrStatus {
    Wait,
    Scanned,
    Confirmed,
    Expired,
    /// IDC failover / rebind redirect (`scaned_but_redirect` / `binded_redirect`).
    Redirect,
    NeedVerifyCode,
    VerifyBlocked,
    Unknown(String),
}

impl QrStatus {
    /// Parse the gateway's `status` string.
    #[must_use]
    pub fn parse(status: &str) -> Self {
        match status {
            "wait" => Self::Wait,
            "scaned" => Self::Scanned,
            "confirmed" => Self::Confirmed,
            "expired" => Self::Expired,
            "scaned_but_redirect" | "binded_redirect" => Self::Redirect,
            "need_verifycode" => Self::NeedVerifyCode,
            "verify_code_blocked" => Self::VerifyBlocked,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether polling should stop (login resolved one way or another).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Confirmed | Self::Expired | Self::VerifyBlocked)
    }

    /// Whether the login succeeded.
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed)
    }
}

/// The QR a user must scan: an opaque `qrcode` id plus a displayable image URL.
#[derive(Debug, Clone)]
pub struct QrStart {
    pub qrcode: String,
    pub image_url: String,
}

/// A logged-in account: the durable bot token and the server-selected API base URL
/// to use for all subsequent gateway calls. Serializable so it can be cached to
/// disk (see [`crate::session_store`]) and survive a restart without re-scanning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinSession {
    pub bot_token: String,
    pub base_url: String,
    pub account_id: String,
    pub user_id: String,
}

/// Start QR login: returns the QR to display.
pub fn start_qr_login(
    agent: &ureq::Agent,
    base_url: &str,
    bot_type: &str,
) -> WeixinResult<QrStart> {
    let url = format!("{base_url}/ilink/bot/get_bot_qrcode?bot_type={bot_type}");
    let value = http::post_json(agent, &url, &[], &json!({ "local_token_list": [] }))?;
    Ok(QrStart {
        qrcode: str_field(&value, "qrcode"),
        image_url: str_field(&value, "qrcode_img_content"),
    })
}

/// Poll the QR status once, returning the parsed status and — on `confirmed` — the
/// resolved [`WeixinSession`]. `bootstrap_base_url` is used as the session base URL
/// only if the gateway does not return its own `baseurl`.
pub fn poll_qr_once(
    agent: &ureq::Agent,
    bootstrap_base_url: &str,
    qrcode: &str,
) -> WeixinResult<(QrStatus, Option<WeixinSession>)> {
    let url = format!("{bootstrap_base_url}/ilink/bot/get_qrcode_status?qrcode={qrcode}");
    let value = http::get_json(agent, &url, &[])?;
    let status = QrStatus::parse(
        value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown"),
    );
    let session = status.is_confirmed().then(|| WeixinSession {
        bot_token: str_field(&value, "bot_token"),
        base_url: {
            let returned = str_field(&value, "baseurl");
            if returned.is_empty() {
                bootstrap_base_url.to_string()
            } else {
                returned
            }
        },
        account_id: str_field(&value, "ilink_bot_id"),
        user_id: str_field(&value, "ilink_user_id"),
    });
    Ok((status, session))
}

/// Run the full QR login: show the QR via `on_qr`, then poll (~1s cadence) until
/// confirmed, expired, blocked, or `overall` elapses.
pub fn qr_login(
    agent: &ureq::Agent,
    bootstrap_base_url: &str,
    bot_type: &str,
    overall: Duration,
    on_qr: impl FnOnce(&QrStart),
) -> WeixinResult<WeixinSession> {
    let start = start_qr_login(agent, bootstrap_base_url, bot_type)?;
    on_qr(&start);
    let deadline = Instant::now() + overall;
    loop {
        if Instant::now() >= deadline {
            return Err(WeixinError::Login("QR login timed out before scan".into()));
        }
        let (status, session) = poll_qr_once(agent, bootstrap_base_url, &start.qrcode)?;
        match status {
            QrStatus::Confirmed => {
                return session
                    .ok_or_else(|| WeixinError::Login("confirmed without a bot_token".into()));
            }
            QrStatus::Expired => return Err(WeixinError::Login("QR code expired".into())),
            QrStatus::VerifyBlocked => {
                return Err(WeixinError::Login("verification blocked".into()));
            }
            _ => std::thread::sleep(Duration::from_millis(1000)),
        }
    }
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_known_statuses() {
        assert_eq!(QrStatus::parse("wait"), QrStatus::Wait);
        assert_eq!(QrStatus::parse("scaned"), QrStatus::Scanned);
        assert_eq!(QrStatus::parse("confirmed"), QrStatus::Confirmed);
        assert_eq!(QrStatus::parse("expired"), QrStatus::Expired);
        assert_eq!(QrStatus::parse("scaned_but_redirect"), QrStatus::Redirect);
        assert_eq!(QrStatus::parse("binded_redirect"), QrStatus::Redirect);
        assert_eq!(QrStatus::parse("need_verifycode"), QrStatus::NeedVerifyCode);
        assert_eq!(
            QrStatus::parse("verify_code_blocked"),
            QrStatus::VerifyBlocked
        );
        assert_eq!(QrStatus::parse("weird"), QrStatus::Unknown("weird".into()));
    }

    #[test]
    fn terminal_and_confirmed_classification() {
        assert!(QrStatus::Confirmed.is_terminal() && QrStatus::Confirmed.is_confirmed());
        assert!(QrStatus::Expired.is_terminal() && !QrStatus::Expired.is_confirmed());
        assert!(QrStatus::VerifyBlocked.is_terminal());
        assert!(!QrStatus::Wait.is_terminal());
        assert!(!QrStatus::Scanned.is_terminal());
        assert!(!QrStatus::Redirect.is_terminal());
    }
}
