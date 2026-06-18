use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nerve_agent::auth::{self, AuthMode, Credential, LoginStart, ProviderId};
use nerve_agent::error::AgentError;
use nerve_core::CancelToken;
use nerve_runtime::{AuthEventKind, RuntimeCommand, RuntimeError, RuntimeEvent};
use serde_json::{Value, json};

const PENDING_LOGIN_TTL: Duration = Duration::from_secs(10 * 60);

type EventEmitter = dyn Fn(RuntimeEvent) + Send + Sync + 'static;

pub(crate) struct AuthManager {
    pending: Arc<Mutex<HashMap<String, PendingLogin>>>,
    emit: Arc<EventEmitter>,
}

impl Default for AuthManager {
    fn default() -> Self {
        Self::new(Arc::new(|_| {}))
    }
}

#[derive(Clone)]
struct PendingLogin {
    start: LoginStart,
    created_at_ms: u64,
}

impl AuthManager {
    pub(crate) fn new(emit: Arc<EventEmitter>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            emit,
        }
    }

    pub(crate) fn handle_command(
        &self,
        command: RuntimeCommand,
        cancel: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        if cancel.is_cancelled() {
            return Err(RuntimeError::cancelled());
        }
        self.sweep_pending(now_ms());
        match command {
            RuntimeCommand::AuthStart { provider } => self.start(&provider, cancel),
            RuntimeCommand::AuthComplete {
                login_id,
                code,
                callback_url,
            } => self.complete(&login_id, code, callback_url, cancel),
            RuntimeCommand::AuthStatus { provider } => self.status(&provider),
            RuntimeCommand::AuthLogout { provider } => self.logout(&provider),
            _ => Err(RuntimeError::adapter("expected auth.* command")),
        }
    }

    fn start(&self, provider: &str, cancel: &CancelToken) -> Result<Value, RuntimeError> {
        let provider = parse_provider(provider)?;
        let strategy = auth::strategy_for(provider);
        let redirect_uri = strategy.default_redirect_uri();
        if cancel.is_cancelled() {
            return Err(RuntimeError::cancelled());
        }
        let start = match strategy.start(&redirect_uri) {
            Ok(start) => start,
            Err(error) => {
                self.emit_auth(provider, AuthEventKind::LoginFailed);
                return Err(agent_runtime_error(error));
            }
        };
        if cancel.is_cancelled() {
            return Err(RuntimeError::cancelled());
        }
        let login_id = format!("login-{}", auth::oauth::random_urlsafe(18));
        let created_at_ms = now_ms();
        self.pending.lock().expect("auth pending lock").insert(
            login_id.clone(),
            PendingLogin {
                start: start.clone(),
                created_at_ms,
            },
        );
        self.schedule_pending_cleanup(login_id.clone(), created_at_ms);
        self.emit_auth(provider, AuthEventKind::LoginPending);
        Ok(json!({
            "login_id": login_id,
            "provider": provider.as_str(),
            "authorize_url": start.authorize_url,
            "redirect_uri": start.redirect_uri,
        }))
    }

    fn complete(
        &self,
        login_id: &str,
        code: Option<String>,
        callback_url: Option<String>,
        cancel: &CancelToken,
    ) -> Result<Value, RuntimeError> {
        let input = callback_url.or(code).ok_or_else(|| {
            RuntimeError::adapter("auth.complete requires `code` or `callback_url`")
        })?;
        let pending = self
            .pending
            .lock()
            .expect("auth pending lock")
            .get(login_id)
            .cloned()
            .ok_or_else(|| RuntimeError::adapter(format!("unknown auth login_id: {login_id}")))?;
        let provider = pending.start.provider;
        let callback = auth::oauth::parse_pasted_callback(input.trim());
        let strategy = auth::strategy_for(provider);
        let credential = match strategy.complete(&pending.start, &callback, cancel) {
            Ok(credential) => credential,
            Err(error) => {
                if !matches!(error, AgentError::Cancelled) {
                    self.emit_auth(provider, AuthEventKind::LoginFailed);
                }
                return Err(agent_runtime_error(error));
            }
        };
        if let Err(error) = auth::save_credential(&credential) {
            self.emit_auth(provider, AuthEventKind::LoginFailed);
            return Err(agent_runtime_error(error));
        }
        self.pending
            .lock()
            .expect("auth pending lock")
            .remove(login_id);
        self.emit_auth(provider, AuthEventKind::LoginCompleted);
        Ok(json!({
            "login_id": login_id,
            "created_at_ms": pending.created_at_ms,
            "credential": credential_status(&credential),
        }))
    }

    fn status(&self, provider: &str) -> Result<Value, RuntimeError> {
        let provider = parse_provider(provider)?;
        match auth::load_credential(provider).map_err(agent_runtime_error)? {
            Some(credential) => Ok(credential_status(&credential)),
            None => Ok(json!({
                "provider": provider.as_str(),
                "status": "not_logged_in",
            })),
        }
    }

    fn logout(&self, provider: &str) -> Result<Value, RuntimeError> {
        let provider = parse_provider(provider)?;
        auth::delete_credential(provider).map_err(agent_runtime_error)?;
        self.pending
            .lock()
            .expect("auth pending lock")
            .retain(|_, pending| pending.start.provider != provider);
        Ok(json!({
            "provider": provider.as_str(),
            "status": "logged_out",
        }))
    }

    fn sweep_pending(&self, now_ms: u64) {
        self.pending
            .lock()
            .expect("auth pending lock")
            .retain(|_, pending| pending_is_fresh(pending.created_at_ms, now_ms));
    }

    fn schedule_pending_cleanup(&self, login_id: String, created_at_ms: u64) {
        let pending = Arc::clone(&self.pending);
        thread::spawn(move || {
            thread::sleep(PENDING_LOGIN_TTL + Duration::from_secs(1));
            pending
                .lock()
                .expect("auth pending lock")
                .retain(|id, pending| {
                    id != &login_id
                        || pending.created_at_ms != created_at_ms
                        || pending_is_fresh(pending.created_at_ms, now_ms())
                });
        });
    }

    fn emit_auth(&self, provider: ProviderId, kind: AuthEventKind) {
        (self.emit)(RuntimeEvent::auth(provider.as_str(), kind));
    }
}

fn pending_is_fresh(created_at_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(created_at_ms) <= pending_login_ttl_ms()
}

fn pending_login_ttl_ms() -> u64 {
    PENDING_LOGIN_TTL.as_millis().try_into().unwrap_or(u64::MAX)
}

fn parse_provider(provider: &str) -> Result<ProviderId, RuntimeError> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Ok(ProviderId::Anthropic),
        "openai" | "chatgpt" | "openai_responses" => Ok(ProviderId::OpenAi),
        "xai" | "grok" => Ok(ProviderId::Xai),
        _ => Err(RuntimeError::adapter(format!(
            "unknown auth provider '{provider}': expected anthropic|openai|xai"
        ))),
    }
}

fn credential_status(credential: &Credential) -> Value {
    json!({
        "provider": credential.provider.as_str(),
        "status": "authenticated",
        "mode": mode_label(credential.mode),
        "base_url": credential.base_url,
        "account_id": credential.account_id,
        "expires_at_unix": credential.expires_at_unix,
    })
}

fn mode_label(mode: AuthMode) -> &'static str {
    match mode {
        AuthMode::ApiKey => "api_key",
        AuthMode::Oauth => "oauth",
    }
}

fn agent_runtime_error(error: AgentError) -> RuntimeError {
    if matches!(error, AgentError::Cancelled) {
        RuntimeError::cancelled()
    } else {
        RuntimeError::adapter(error.to_string())
    }
}

fn now_ms() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_provider_accepts_aliases() {
        assert_eq!(
            parse_provider("claude").expect("claude"),
            ProviderId::Anthropic
        );
        assert_eq!(
            parse_provider("chatgpt").expect("chatgpt"),
            ProviderId::OpenAi
        );
        assert_eq!(parse_provider("grok").expect("grok"), ProviderId::Xai);
        assert!(parse_provider("unknown").is_err());
    }

    #[test]
    fn pending_ttl_marks_old_entries_stale() {
        let now = pending_login_ttl_ms() + 10_000;
        assert!(pending_is_fresh(now - pending_login_ttl_ms(), now));
        assert!(!pending_is_fresh(now - pending_login_ttl_ms() - 1, now));
    }

    #[test]
    fn sweep_pending_removes_stale_entries() {
        let manager = AuthManager::default();
        manager.pending.lock().expect("auth pending lock").insert(
            "old".to_string(),
            PendingLogin {
                start: auth::strategy_for(ProviderId::OpenAi)
                    .start("http://127.0.0.1/callback")
                    .expect("login start"),
                created_at_ms: 1,
            },
        );
        manager.sweep_pending(pending_login_ttl_ms() + 2);
        assert!(
            manager
                .pending
                .lock()
                .expect("auth pending lock")
                .is_empty()
        );
    }

    #[test]
    fn auth_start_emits_pending_event() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&events);
        let manager = AuthManager::new(Arc::new(move |event| {
            captured.lock().expect("events lock").push(event);
        }));
        let result = manager
            .handle_command(
                RuntimeCommand::AuthStart {
                    provider: "openai".to_string(),
                },
                &CancelToken::never(),
            )
            .expect("auth start");
        assert_eq!(result["provider"], "openai");
        let events = events.lock().expect("events lock");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            RuntimeEvent::auth("openai", AuthEventKind::LoginPending)
        );
    }

    #[test]
    fn credential_status_does_not_include_secrets() {
        let credential = Credential {
            provider: ProviderId::OpenAi,
            mode: AuthMode::Oauth,
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            expires_at_unix: Some(123),
            account_id: Some("acct".into()),
            base_url: ProviderId::OpenAi.default_base_url().to_string(),
        };
        let status = credential_status(&credential);
        assert_eq!(status["provider"], "openai");
        assert_eq!(status["mode"], "oauth");
        assert!(status.get("access_token").is_none());
        assert!(status.get("refresh_token").is_none());
    }
}
