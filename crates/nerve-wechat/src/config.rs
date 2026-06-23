//! Runtime configuration for the `nerve-wechat` bridge binary, read from the
//! environment so no secrets live on the command line.
//!
//! | env var | meaning | default |
//! |---|---|---|
//! | `NERVE_WECHAT_ROOT` | workspace root the delegated agent is confined to | required |
//! | `NERVE_WECHAT_BOT_TYPE` | iLink `bot_type` for your bot registration | required |
//! | `NERVE_WECHAT_OWNERS` | comma-separated WeChat user ids allowed to drive the agent | required (empty denies all) |
//! | `NERVE_BIN` | the `nerve` binary to spawn the daemon | `nerve` |
//! | `NERVE_WECHAT_AGENT` | delegate agent (`claude`/`codex`/`gemini`) | `claude` |
//! | `NERVE_WECHAT_AUTONOMY` | `read_only`/`edit`/`full` granted to delegated turns | `read_only` |
//! | `NERVE_WECHAT_BASE_URL` | login bootstrap host | iLink default |
//! | `NERVE_WECHAT_STATE` | path to cache the logged-in session (skips QR on restart; 0o600) | (no persistence) |
//! | `NERVE_WECHAT_TOKEN` + `..._SESSION_BASE_URL` + `..._USER_ID` [+ `..._ACCOUNT_ID`] | a saved session to skip QR login | (QR login) |

use crate::gateway::DEFAULT_BASE_URL;
use crate::login::WeixinSession;
use nerve_proto::DelegateAutonomy;
use std::path::PathBuf;

/// Bridge configuration.
pub struct WechatConfig {
    pub nerve_bin: String,
    pub root: PathBuf,
    pub agent: String,
    pub autonomy: DelegateAutonomy,
    pub bot_type: String,
    pub bootstrap_base_url: String,
    pub owners: Vec<String>,
    /// A pre-obtained session (from env) to skip the QR flow entirely.
    pub preset_session: Option<WeixinSession>,
    /// Where to cache the logged-in session on disk so a restart skips QR login
    /// (`NERVE_WECHAT_STATE`). `None` disables persistence.
    pub state_path: Option<PathBuf>,
}

impl WechatConfig {
    /// Load from the environment, validating the required fields.
    pub fn from_env() -> Result<Self, String> {
        let root = required("NERVE_WECHAT_ROOT")?;
        let bot_type = required("NERVE_WECHAT_BOT_TYPE")?;
        Ok(Self {
            nerve_bin: opt("NERVE_BIN").unwrap_or_else(|| "nerve".to_string()),
            root: PathBuf::from(root),
            agent: opt("NERVE_WECHAT_AGENT").unwrap_or_else(|| "claude".to_string()),
            autonomy: parse_autonomy(
                opt("NERVE_WECHAT_AUTONOMY")
                    .as_deref()
                    .unwrap_or("read_only"),
            ),
            bot_type,
            bootstrap_base_url: opt("NERVE_WECHAT_BASE_URL")
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            owners: parse_owners(opt("NERVE_WECHAT_OWNERS").as_deref().unwrap_or("")),
            preset_session: preset_session_from_env(),
            state_path: opt("NERVE_WECHAT_STATE").map(PathBuf::from),
        })
    }
}

fn opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.is_empty())
}

fn required(key: &str) -> Result<String, String> {
    opt(key).ok_or_else(|| format!("missing required env var {key}"))
}

/// Parse an autonomy string, defaulting to the most restricted posture.
fn parse_autonomy(value: &str) -> DelegateAutonomy {
    match value {
        "edit" => DelegateAutonomy::Edit,
        "full" => DelegateAutonomy::Full,
        _ => DelegateAutonomy::ReadOnly,
    }
}

/// Split a comma-separated owner list, trimming blanks.
fn parse_owners(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A saved session is used only when token + base URL + user id are all present.
fn preset_session_from_env() -> Option<WeixinSession> {
    let bot_token = opt("NERVE_WECHAT_TOKEN")?;
    let base_url = opt("NERVE_WECHAT_SESSION_BASE_URL")?;
    let user_id = opt("NERVE_WECHAT_USER_ID")?;
    let account_id = opt("NERVE_WECHAT_ACCOUNT_ID").unwrap_or_else(|| user_id.clone());
    Some(WeixinSession {
        bot_token,
        base_url,
        account_id,
        user_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autonomy_parsing_defaults_to_read_only() {
        assert_eq!(parse_autonomy("edit"), DelegateAutonomy::Edit);
        assert_eq!(parse_autonomy("full"), DelegateAutonomy::Full);
        assert_eq!(parse_autonomy("read_only"), DelegateAutonomy::ReadOnly);
        assert_eq!(parse_autonomy("nonsense"), DelegateAutonomy::ReadOnly);
    }

    #[test]
    fn owners_split_and_trim() {
        assert_eq!(parse_owners(" u1 , u2 ,,u3 "), vec!["u1", "u2", "u3"]);
        assert!(parse_owners("").is_empty());
    }
}
