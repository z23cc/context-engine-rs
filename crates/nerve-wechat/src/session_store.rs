//! On-disk persistence of a logged-in [`WeixinSession`], so the bridge survives a
//! restart without re-scanning the QR code.
//!
//! The session carries a long-lived `bot_token`, so the file is written
//! owner-only (`0o600` on Unix). It is a cache, not the source of truth: a missing,
//! unreadable, or corrupt file simply falls back to QR login.

use crate::login::WeixinSession;
use std::fs;
use std::io;
use std::path::Path;

/// Load a saved session from `path`. Returns `None` when the file is absent,
/// unreadable, or not a valid session (the caller then falls back to QR login).
#[must_use]
pub fn load_session(path: &Path) -> Option<WeixinSession> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist `session` to `path` as owner-only JSON, creating parent directories.
///
/// # Errors
/// Returns an error if the parent directory or file cannot be created/written.
pub fn save_session(path: &Path, session: &WeixinSession) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(session)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    fs::write(path, &json)?;
    set_owner_only(path)
}

/// Restrict `path` to owner read/write (`0o600`). No-op on non-Unix platforms,
/// where the OS account model differs.
#[cfg(unix)]
fn set_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WeixinSession {
        WeixinSession {
            bot_token: "tok-123".into(),
            base_url: "https://host.example".into(),
            account_id: "acct@im.bot".into(),
            user_id: "u_self".into(),
        }
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("wechat.json");
        let session = sample();
        save_session(&path, &session).expect("save");
        let loaded = load_session(&path).expect("load");
        assert_eq!(loaded.bot_token, session.bot_token);
        assert_eq!(loaded.base_url, session.base_url);
        assert_eq!(loaded.account_id, session.account_id);
        assert_eq!(loaded.user_id, session.user_id);
    }

    #[test]
    fn missing_or_corrupt_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_session(&dir.path().join("absent.json")).is_none());
        let corrupt = dir.path().join("corrupt.json");
        fs::write(&corrupt, b"not json").expect("write");
        assert!(load_session(&corrupt).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wechat.json");
        save_session(&path, &sample()).expect("save");
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "session file must be owner-only");
    }
}
