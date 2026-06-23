//! The bridge core: turn inbound WeChat messages into nerve work and stream the
//! reply back, with the account-safety guards that make a personal-account bot
//! safe to run.
//!
//! Decision logic here is pure and fully unit-tested against fakes; the I/O edges
//! are the [`WeixinGateway`] (inbox/send) and [`NerveControl`] (the nerve daemon
//! runtime-protocol client) seams. The real `NerveControl` over `/rpc` + SSE and
//! the `nerve wechat` binary are the next slice.
//!
//! Safety invariants:
//! - **Sender allowlist** — only WeChat user ids you list may drive the agent; an
//!   empty allowlist denies everyone (fail-closed), so a misconfigured bridge can
//!   never execute commands from an arbitrary stranger.
//! - **No self-echo** — messages from the bot's own user id are ignored.
//! - **Dedup** — each `message_id` is handled at most once.

use crate::error::WeixinError;
use crate::gateway::WeixinGateway;
use crate::types::WeixinMessage;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;
use thiserror::Error;

/// Consecutive transient failures tolerated in the run loop before giving up (the
/// supervisor/human then restarts — the persisted session makes that cheap).
const MAX_CONSECUTIVE_ERRORS: u32 = 5;
/// Backoff between transient-error retries in the run loop.
const RETRY_BACKOFF: Duration = Duration::from_secs(3);

/// Reply sent when an allowed owner sends a media-only message (image/file/voice):
/// media relay is not implemented yet, so acknowledge rather than silently drop.
const UNSUPPORTED_MEDIA_REPLY: &str =
    "I can only act on text messages right now — please describe the task in text.";

/// A bridge failure.
#[derive(Debug, Error)]
pub enum BridgeError {
    #[error(transparent)]
    Weixin(#[from] WeixinError),
    #[error("nerve control error: {0}")]
    Nerve(String),
}

/// The reply produced by handling a user message: the text to send back and the
/// nerve session id to remember for this chat (so follow-ups steer it).
#[derive(Debug, Clone)]
pub struct NerveReply {
    pub session_id: String,
    pub text: String,
}

/// The nerve-daemon side the bridge drives. The production impl maps `handle` onto
/// `delegate.start` (when `existing` is `None`) / `delegate.steer` (to resume),
/// defaulting to read-only autonomy, over the runtime protocol.
pub trait NerveControl {
    /// Handle a user message for a chat. `existing` is the chat's nerve session id
    /// if one is active (`None` → start a new session). Returns the reply text plus
    /// the (possibly new) session id to remember.
    fn handle(&self, existing: Option<&str>, text: &str) -> Result<NerveReply, BridgeError>;
}

/// Who may drive the agent. Empty = deny everyone (fail-closed).
#[derive(Debug, Clone, Default)]
pub struct SenderAllowlist {
    owners: BTreeSet<String>,
}

impl SenderAllowlist {
    /// Build an allowlist from a set of WeChat user ids.
    #[must_use]
    pub fn new(owners: impl IntoIterator<Item = String>) -> Self {
        Self {
            owners: owners.into_iter().collect(),
        }
    }

    /// Whether `user_id` is permitted to drive the agent.
    #[must_use]
    pub fn allows(&self, user_id: &str) -> bool {
        self.owners.contains(user_id)
    }

    /// Whether the allowlist is empty (denies everyone).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }
}

/// The WeChat-conversation → nerve-session mapping.
#[derive(Debug, Default)]
pub struct SessionMap {
    map: HashMap<String, String>,
}

impl SessionMap {
    fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    fn insert(&mut self, key: String, session_id: String) {
        self.map.insert(key, session_id);
    }
}

/// A stable conversation key: account + (group or direct peer).
#[must_use]
pub fn chat_key(account_id: &str, msg: &WeixinMessage) -> String {
    if msg.is_group() {
        format!("{account_id}:g:{}", msg.group_id)
    } else {
        format!("{account_id}:d:{}", msg.from_user_id)
    }
}

/// Bridges one logged-in WeChat account to a nerve daemon.
pub struct Bridge<G: WeixinGateway, N: NerveControl> {
    gateway: G,
    nerve: N,
    allowlist: SenderAllowlist,
    account_id: String,
    bot_user_id: String,
    sessions: SessionMap,
    seen: BTreeSet<String>,
    cursor: String,
}

impl<G: WeixinGateway, N: NerveControl> Bridge<G, N> {
    /// Build a bridge. `bot_user_id` is the account's own user id (used to ignore
    /// self-echoes); `account_id` namespaces the per-chat session keys.
    pub fn new(
        gateway: G,
        nerve: N,
        allowlist: SenderAllowlist,
        account_id: impl Into<String>,
        bot_user_id: impl Into<String>,
    ) -> Self {
        Self {
            gateway,
            nerve,
            allowlist,
            account_id: account_id.into(),
            bot_user_id: bot_user_id.into(),
            sessions: SessionMap::default(),
            seen: BTreeSet::new(),
            cursor: String::new(),
        }
    }

    /// Whether `msg` should drive the agent: not a self-echo, not already handled,
    /// from an allowed sender, and carrying text.
    fn accepts(&mut self, msg: &WeixinMessage) -> bool {
        if msg.from_user_id.is_empty() || msg.from_user_id == self.bot_user_id {
            return false;
        }
        if !msg.message_id.is_empty() && !self.seen.insert(msg.message_id.clone()) {
            return false;
        }
        self.allowlist.allows(&msg.from_user_id)
    }

    /// Process one inbound message; returns whether it drove the agent.
    fn handle_message(&mut self, msg: &WeixinMessage) -> Result<bool, BridgeError> {
        if !self.accepts(msg) {
            return Ok(false);
        }
        let Some(text) = msg.text() else {
            // Acknowledge an allowed owner's media-only message instead of silently
            // dropping it (text-only for now).
            self.gateway
                .send_text(&msg.from_user_id, &msg.session_id, UNSUPPORTED_MEDIA_REPLY)?;
            return Ok(false);
        };
        let key = chat_key(&self.account_id, msg);
        let reply = self.nerve.handle(self.sessions.get(&key), &text)?;
        self.sessions.insert(key, reply.session_id);
        self.gateway
            .send_text(&msg.from_user_id, &msg.session_id, &reply.text)?;
        Ok(true)
    }

    /// Long-poll once and handle every returned message; returns how many drove the
    /// agent. The cursor only advances when the gateway returns a non-empty one (an
    /// empty buf is a poll timeout — keep the previous cursor).
    pub fn poll_once(&mut self) -> Result<usize, BridgeError> {
        let resp = self.gateway.get_updates(&self.cursor)?;
        if !resp.get_updates_buf.is_empty() {
            self.cursor = resp.get_updates_buf.clone();
        }
        let mut handled = 0;
        for msg in &resp.msgs {
            if self.handle_message(msg)? {
                handled += 1;
            }
        }
        Ok(handled)
    }

    /// Run the long-poll loop. Transient transport errors (network blips) are
    /// retried with backoff up to [`MAX_CONSECUTIVE_ERRORS`] in a row; a fatal error
    /// (gateway/session/parse/daemon) returns so the supervisor can restart (the
    /// persisted session makes a restart cheap). The gateway long-poll blocks, so a
    /// healthy loop is not busy.
    pub fn run(&mut self) -> Result<(), BridgeError> {
        let mut consecutive = 0u32;
        loop {
            match self.poll_once() {
                Ok(_) => consecutive = 0,
                Err(err) if is_retryable(&err) && consecutive < MAX_CONSECUTIVE_ERRORS => {
                    consecutive += 1;
                    eprintln!(
                        "nerve-wechat: transient error ({err}); retry {consecutive}/{MAX_CONSECUTIVE_ERRORS}"
                    );
                    std::thread::sleep(RETRY_BACKOFF);
                }
                Err(err) => return Err(err),
            }
        }
    }
}

/// Whether a run-loop error is a transient network blip worth retrying. Only a
/// transport error qualifies; a gateway code (e.g. session timeout), a parse
/// failure, a login error, or a daemon-side error is fatal (needs a restart /
/// re-login, not a blind retry).
fn is_retryable(err: &BridgeError) -> bool {
    matches!(err, BridgeError::Weixin(WeixinError::Transport(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WeixinResult;
    use crate::types::{GetUpdatesResp, MessageItem};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// A gateway whose inbox is a scripted queue; records what was sent.
    struct FakeGateway {
        inbox: RefCell<VecDeque<GetUpdatesResp>>,
        sent: RefCell<Vec<(String, String, String)>>,
    }

    impl FakeGateway {
        fn with(updates: Vec<GetUpdatesResp>) -> Self {
            Self {
                inbox: RefCell::new(updates.into_iter().collect()),
                sent: RefCell::new(Vec::new()),
            }
        }
    }

    impl WeixinGateway for FakeGateway {
        fn get_updates(&self, _cursor: &str) -> WeixinResult<GetUpdatesResp> {
            Ok(self.inbox.borrow_mut().pop_front().unwrap_or_default())
        }
        fn send_text(&self, to: &str, session_id: &str, text: &str) -> WeixinResult<()> {
            self.sent
                .borrow_mut()
                .push((to.to_string(), session_id.to_string(), text.to_string()));
            Ok(())
        }
    }

    /// A nerve control that records (existing, text) calls and echoes a fixed session.
    struct FakeNerve {
        calls: RefCell<Vec<(Option<String>, String)>>,
    }

    impl FakeNerve {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl NerveControl for FakeNerve {
        fn handle(&self, existing: Option<&str>, text: &str) -> Result<NerveReply, BridgeError> {
            self.calls
                .borrow_mut()
                .push((existing.map(str::to_string), text.to_string()));
            Ok(NerveReply {
                session_id: "sess-1".to_string(),
                text: format!("ack: {text}"),
            })
        }
    }

    fn msg(id: &str, from: &str, text: &str) -> WeixinMessage {
        WeixinMessage {
            message_id: id.to_string(),
            from_user_id: from.to_string(),
            session_id: format!("wxsess-{from}"),
            item_list: vec![MessageItem::text(text)],
            ..Default::default()
        }
    }

    fn updates(cursor: &str, msgs: Vec<WeixinMessage>) -> GetUpdatesResp {
        GetUpdatesResp {
            ret: 0,
            msgs,
            get_updates_buf: cursor.to_string(),
        }
    }

    fn bridge(owners: &[&str], updates: Vec<GetUpdatesResp>) -> Bridge<FakeGateway, FakeNerve> {
        Bridge::new(
            FakeGateway::with(updates),
            FakeNerve::new(),
            SenderAllowlist::new(owners.iter().map(|s| s.to_string())),
            "acct",
            "bot_self",
        )
    }

    #[test]
    fn allowed_owner_text_drives_nerve_and_replies() {
        let mut b = bridge(
            &["u_owner"],
            vec![updates("c1", vec![msg("m1", "u_owner", "fix it")])],
        );
        let handled = b.poll_once().expect("poll");
        assert_eq!(handled, 1);
        assert_eq!(b.cursor, "c1");
        // nerve called with no existing session, then the reply is sent back to the sender.
        assert_eq!(
            b.nerve.calls.borrow().as_slice(),
            &[(None, "fix it".to_string())]
        );
        assert_eq!(
            b.gateway.sent.borrow().as_slice(),
            &[(
                "u_owner".to_string(),
                "wxsess-u_owner".to_string(),
                "ack: fix it".to_string()
            )]
        );
    }

    #[test]
    fn non_owner_is_ignored_fail_closed() {
        let mut b = bridge(
            &["u_owner"],
            vec![updates("c1", vec![msg("m1", "stranger", "rm -rf")])],
        );
        assert_eq!(b.poll_once().expect("poll"), 0);
        assert!(
            b.nerve.calls.borrow().is_empty(),
            "stranger must not reach nerve"
        );
        assert!(b.gateway.sent.borrow().is_empty());
    }

    #[test]
    fn empty_allowlist_denies_everyone() {
        let mut b = bridge(&[], vec![updates("c1", vec![msg("m1", "u_owner", "hi")])]);
        assert_eq!(b.poll_once().expect("poll"), 0);
        assert!(b.nerve.calls.borrow().is_empty());
    }

    #[test]
    fn self_echo_is_ignored() {
        let mut b = bridge(
            &["bot_self"],
            vec![updates("c1", vec![msg("m1", "bot_self", "echo")])],
        );
        assert_eq!(b.poll_once().expect("poll"), 0);
        assert!(b.nerve.calls.borrow().is_empty());
    }

    #[test]
    fn media_only_message_from_owner_is_acknowledged_not_run() {
        let mut media = msg("m1", "u_owner", "");
        media.item_list = vec![MessageItem {
            item_type: crate::types::item_type::IMAGE,
            text_item: None,
        }];
        let mut b = bridge(&["u_owner"], vec![updates("c1", vec![media])]);
        // Not handled (the agent never runs)...
        assert_eq!(b.poll_once().expect("poll"), 0);
        assert!(b.nerve.calls.borrow().is_empty());
        // ...but the owner gets a friendly text-only acknowledgement (not silently dropped).
        let sent = b.gateway.sent.borrow();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "u_owner");
        assert!(sent[0].2.contains("text messages"));
    }

    #[test]
    fn only_transport_errors_are_retryable() {
        assert!(is_retryable(&BridgeError::Weixin(WeixinError::Transport(
            "connection reset".into()
        ))));
        // A gateway code (session timeout etc.), parse, or daemon error is fatal.
        assert!(!is_retryable(&BridgeError::Weixin(WeixinError::Gateway {
            ret: -14
        })));
        assert!(!is_retryable(&BridgeError::Nerve("daemon closed".into())));
    }

    #[test]
    fn duplicate_message_id_handled_once() {
        let dup = msg("m1", "u_owner", "twice");
        let mut b = bridge(
            &["u_owner"],
            vec![updates("c1", vec![dup.clone()]), updates("c2", vec![dup])],
        );
        assert_eq!(b.poll_once().expect("poll1"), 1);
        assert_eq!(
            b.poll_once().expect("poll2"),
            0,
            "same message_id must not re-run"
        );
        assert_eq!(b.nerve.calls.borrow().len(), 1);
    }

    #[test]
    fn follow_up_reuses_the_chat_session() {
        let mut b = bridge(
            &["u_owner"],
            vec![
                updates("c1", vec![msg("m1", "u_owner", "first")]),
                updates("c2", vec![msg("m2", "u_owner", "second")]),
            ],
        );
        b.poll_once().expect("poll1");
        b.poll_once().expect("poll2");
        let calls = b.nerve.calls.borrow();
        assert_eq!(calls[0], (None, "first".to_string()));
        // Second message in the same chat resumes the remembered session.
        assert_eq!(calls[1], (Some("sess-1".to_string()), "second".to_string()));
    }

    #[test]
    fn empty_poll_buf_preserves_cursor() {
        let mut b = bridge(
            &["u_owner"],
            vec![updates("c1", vec![]), updates("", vec![])],
        );
        b.poll_once().expect("poll1");
        assert_eq!(b.cursor, "c1");
        b.poll_once().expect("poll2 (timeout)");
        assert_eq!(b.cursor, "c1", "empty buf is a timeout — keep the cursor");
    }
}
