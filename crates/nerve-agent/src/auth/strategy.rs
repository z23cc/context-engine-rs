//! Per-provider login/refresh strategies.
//!
//! Each strategy runs the authorization-code + PKCE flow over a loopback
//! redirect (with a manual-paste fallback) and exchanges the code for a
//! [`Credential`] in `Oauth` mode, then refreshes via the stored refresh token.
//! Endpoints, client ids, and scopes are ported from the oh-my-pi OAuth
//! registry. Request-time impersonation headers are *not* set here — those
//! belong to the provider request path; this module only obtains, refreshes,
//! and shapes tokens.

use std::time::Duration;

use serde_json::{Value, json};

use super::oauth::{
    self, Pkce, optional_str, post_token_form, post_token_form_cancel, post_token_json,
    post_token_json_cancel, required_str,
};
use super::{AuthMode, AuthStrategy, Credential, LoginOptions, LoginStart, ProviderId};
use crate::error::{AgentError, AgentResult};

#[path = "strategy_flow.rs"]
mod strategy_flow;
use strategy_flow::*;

/// Client-side expiry skew (seconds): refresh a little before real expiry.
const EXPIRY_SKEW_SECS: u64 = 5 * 60;

/// Build an [`AuthStrategy`] for `provider`.
pub fn strategy_for(provider: ProviderId) -> Box<dyn AuthStrategy> {
    match provider {
        ProviderId::Anthropic => Box::new(AnthropicAuth),
        ProviderId::OpenAi => Box::new(OpenAiAuth),
        ProviderId::Xai => Box::new(XaiAuth),
    }
}

/// Build an API-key [`Credential`] (no OAuth). `base_url` falls back to the
/// provider default when empty.
pub fn from_api_key(provider: ProviderId, key: &str, base_url: Option<&str>) -> Credential {
    let base_url = base_url
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| provider.default_base_url())
        .trim_end_matches('/')
        .to_string();
    Credential {
        provider,
        mode: AuthMode::ApiKey,
        access_token: key.trim().to_string(),
        refresh_token: None,
        expires_at_unix: None,
        account_id: None,
        base_url,
    }
}

/// Fixed parameters describing a loopback OAuth endpoint configuration.
struct Loopback {
    host: &'static str,
    port: u16,
    path: &'static str,
    /// Allow falling back to an ephemeral port when `port` is busy.
    allow_fallback: bool,
}

// ----------------------------------------------------------------------------
// Anthropic (Claude Pro/Max)
// ----------------------------------------------------------------------------

const ANTHROPIC_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const ANTHROPIC_TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
// base64("9d1c250a-e61b-44d9-88ed-5944d1962f5e"); kept decoded to avoid a build dep.
const ANTHROPIC_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const ANTHROPIC_SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
const ANTHROPIC_REDIRECT_HOST: &str = "localhost";
const ANTHROPIC_REDIRECT_PORT: u16 = 54_545;
const ANTHROPIC_REDIRECT_PATH: &str = "/callback";

struct AnthropicAuth;

impl AnthropicAuth {
    fn loopback() -> Loopback {
        Loopback {
            host: ANTHROPIC_REDIRECT_HOST,
            port: ANTHROPIC_REDIRECT_PORT,
            path: ANTHROPIC_REDIRECT_PATH,
            allow_fallback: true,
        }
    }

    fn authorize_url(redirect_uri: &str, state: &str, pkce: &Pkce) -> AgentResult<String> {
        let mut url = url::Url::parse(ANTHROPIC_AUTHORIZE_URL)
            .map_err(|err| AgentError::Auth(err.to_string()))?;
        url.query_pairs_mut()
            .append_pair("code", "true")
            .append_pair("client_id", ANTHROPIC_CLIENT_ID)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", ANTHROPIC_SCOPES)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        Ok(url.to_string())
    }

    fn credential_from_token(value: &Value) -> AgentResult<Credential> {
        let access_token = required_str(value, "access_token", "Anthropic token")?;
        let refresh_token = oauth::optional_str(value, "refresh_token");
        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                AgentError::Parse("Anthropic token response missing `expires_in`".into())
            })?;
        let account = value.get("account");
        let account_id = account
            .and_then(|account| account.get("uuid"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned);
        Ok(Credential {
            provider: ProviderId::Anthropic,
            mode: AuthMode::Oauth,
            access_token,
            refresh_token,
            expires_at_unix: Some(oauth::expires_at(expires_in, EXPIRY_SKEW_SECS)),
            account_id,
            base_url: ProviderId::Anthropic.default_base_url().to_string(),
        })
    }
}

impl AuthStrategy for AnthropicAuth {
    fn provider(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    fn default_redirect_uri(&self) -> String {
        default_redirect_uri(&Self::loopback())
    }

    fn start(&self, redirect_uri: &str) -> AgentResult<LoginStart> {
        make_login_start(ProviderId::Anthropic, redirect_uri, Self::authorize_url)
    }

    fn complete(
        &self,
        start: &LoginStart,
        callback: &oauth::OAuthCallback,
        cancel: &nerve_core::CancelToken,
    ) -> AgentResult<Credential> {
        ensure_login_provider(start, ProviderId::Anthropic)?;
        let raw_code = validated_code(start, callback)?;
        // Anthropic validates the echoed CSRF `state` on exchange, so the flow's
        // real `state` must be sent; an empty string is rejected HTTP 400. A
        // combined `code#state` callback's non-empty fragment overrides it.
        let (code, state) = anthropic_code_state(raw_code, &start.state);
        let body = json!({
            "grant_type": "authorization_code",
            "client_id": ANTHROPIC_CLIENT_ID,
            "code": code,
            "state": state,
            "redirect_uri": start.redirect_uri,
            "code_verifier": start.verifier,
        });
        let value = post_token_json_cancel(ANTHROPIC_TOKEN_URL, &[], &body, cancel)?;
        Self::credential_from_token(&value)
    }

    fn login(&self, opts: &LoginOptions) -> AgentResult<Credential> {
        run_interactive_login(self, &Self::loopback(), opts)
    }

    fn refresh(&self, cred: &Credential) -> AgentResult<Credential> {
        let refresh_token = cred
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AgentError::Auth("Anthropic credential has no refresh_token".into()))?;
        let body = json!({
            "grant_type": "refresh_token",
            "client_id": ANTHROPIC_CLIENT_ID,
            "refresh_token": refresh_token,
        });
        let headers = [
            ("anthropic-beta".to_string(), ANTHROPIC_BETA.to_string()),
            (
                "User-Agent".to_string(),
                "anthropic-sdk-typescript/0.94.0 userOAuthProvider".to_string(),
            ),
        ];
        let value = post_token_json(ANTHROPIC_TOKEN_URL, &headers, &body)?;
        let mut refreshed = Self::credential_from_token(&value)?;
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some(refresh_token.to_string());
        }
        Ok(refreshed)
    }
}

/// Resolve the `(code, state)` to send on the Anthropic token exchange.
///
/// Anthropic validates the echoed CSRF `state`, so the flow's real `state` must
/// be sent — sending an empty string is rejected with HTTP 400 "Invalid request
/// format". When a consent flow hands back a combined `code#state` string, a
/// non-empty `#state` fragment takes precedence (matches the reference flow).
fn anthropic_code_state(raw_code: String, default_state: &str) -> (String, String) {
    match raw_code.split_once('#') {
        Some((code, fragment)) if !fragment.is_empty() => (code.to_string(), fragment.to_string()),
        _ => (raw_code, default_state.to_string()),
    }
}

// ----------------------------------------------------------------------------
// OpenAI (ChatGPT / Codex)
// ----------------------------------------------------------------------------

const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OPENAI_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const OPENAI_ORIGINATOR: &str = "nerve";
/// ChatGPT/Codex subscription API base. OAuth tokens use the ChatGPT backend,
/// not the platform API (`api.openai.com`), which 401s on subscription tokens.
const OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_REDIRECT_HOST: &str = "localhost";
const OPENAI_REDIRECT_PORT: u16 = 1455;
const OPENAI_REDIRECT_PATH: &str = "/auth/callback";
const OPENAI_JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const OPENAI_JWT_PROFILE_CLAIM: &str = "https://api.openai.com/profile";

struct OpenAiAuth;

impl OpenAiAuth {
    fn loopback() -> Loopback {
        Loopback {
            host: OPENAI_REDIRECT_HOST,
            port: OPENAI_REDIRECT_PORT,
            path: OPENAI_REDIRECT_PATH,
            // OpenAI only allows the fixed redirect; never fall back.
            allow_fallback: false,
        }
    }

    fn authorize_url(redirect_uri: &str, state: &str, pkce: &Pkce) -> AgentResult<String> {
        let mut url = url::Url::parse(OPENAI_AUTHORIZE_URL)
            .map_err(|err| AgentError::Auth(err.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", OPENAI_CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", OPENAI_SCOPE)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("originator", OPENAI_ORIGINATOR);
        Ok(url.to_string())
    }

    /// Extract `(account_id, email)` from the access token's JWT claims.
    fn token_profile(access_token: &str) -> (Option<String>, Option<String>) {
        let Some(claims) = oauth::decode_jwt_claims(access_token) else {
            return (None, None);
        };
        let account_id = claims
            .get(OPENAI_JWT_AUTH_CLAIM)
            .and_then(|auth| auth.get("chatgpt_account_id"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned);
        let email = claims
            .get(OPENAI_JWT_PROFILE_CLAIM)
            .and_then(|profile| profile.get("email"))
            .and_then(Value::as_str)
            .map(|email| email.trim().to_lowercase())
            .filter(|email| !email.is_empty());
        (account_id, email)
    }

    fn credential_from_token(value: &Value, require_account: bool) -> AgentResult<Credential> {
        let access_token = required_str(value, "access_token", "OpenAI token")?;
        let refresh_token = optional_str(value, "refresh_token");
        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                AgentError::Parse("OpenAI token response missing `expires_in`".into())
            })?;
        let (account_id, _email) = Self::token_profile(&access_token);
        if require_account && account_id.is_none() {
            return Err(AgentError::Auth(
                "OpenAI token did not contain a chatgpt_account_id".into(),
            ));
        }
        Ok(Credential {
            provider: ProviderId::OpenAi,
            mode: AuthMode::Oauth,
            access_token,
            refresh_token,
            expires_at_unix: Some(oauth::expires_at(expires_in, EXPIRY_SKEW_SECS)),
            account_id,
            base_url: OPENAI_CODEX_BASE_URL.to_string(),
        })
    }
}

impl AuthStrategy for OpenAiAuth {
    fn provider(&self) -> ProviderId {
        ProviderId::OpenAi
    }

    fn default_redirect_uri(&self) -> String {
        default_redirect_uri(&Self::loopback())
    }

    fn start(&self, redirect_uri: &str) -> AgentResult<LoginStart> {
        make_login_start(ProviderId::OpenAi, redirect_uri, Self::authorize_url)
    }

    fn complete(
        &self,
        start: &LoginStart,
        callback: &oauth::OAuthCallback,
        cancel: &nerve_core::CancelToken,
    ) -> AgentResult<Credential> {
        ensure_login_provider(start, ProviderId::OpenAi)?;
        let code = validated_code(start, callback)?;
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", OPENAI_CLIENT_ID),
            ("code", code.as_str()),
            ("code_verifier", start.verifier.as_str()),
            ("redirect_uri", start.redirect_uri.as_str()),
        ];
        let value = post_token_form_cancel(OPENAI_TOKEN_URL, &form, cancel)?;
        Self::credential_from_token(&value, true)
    }

    fn login(&self, opts: &LoginOptions) -> AgentResult<Credential> {
        run_interactive_login(self, &Self::loopback(), opts)
    }

    fn refresh(&self, cred: &Credential) -> AgentResult<Credential> {
        let refresh_token = cred
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AgentError::Auth("OpenAI credential has no refresh_token".into()))?;
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", OPENAI_CLIENT_ID),
        ];
        let value = post_token_form(OPENAI_TOKEN_URL, &form)?;
        let mut refreshed = Self::credential_from_token(&value, false)?;
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some(refresh_token.to_string());
        }
        if refreshed.account_id.is_none() {
            refreshed.account_id = cred.account_id.clone();
        }
        Ok(refreshed)
    }
}

// ----------------------------------------------------------------------------
// xAI (Grok / SuperGrok)
// ----------------------------------------------------------------------------

const XAI_DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const XAI_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const XAI_SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const XAI_REDIRECT_HOST: &str = "127.0.0.1";
const XAI_REDIRECT_PORT: u16 = 56_121;
const XAI_REDIRECT_PATH: &str = "/callback";
const XAI_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

struct XaiAuth;

/// The two endpoints xAI advertises via OIDC discovery.
struct XaiDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

impl XaiAuth {
    fn loopback() -> Loopback {
        Loopback {
            host: XAI_REDIRECT_HOST,
            port: XAI_REDIRECT_PORT,
            path: XAI_REDIRECT_PATH,
            // xAI's redirect_uri allowlist requires the fixed loopback port.
            allow_fallback: false,
        }
    }

    /// Fetch and validate xAI's OIDC discovery document.
    fn discover() -> AgentResult<XaiDiscovery> {
        let agent = crate::provider::http::http_agent(XAI_DISCOVERY_TIMEOUT);
        let mut response = agent
            .get(XAI_DISCOVERY_URL)
            .header("Accept", "application/json")
            .call()
            .map_err(|err| AgentError::Http(format!("xAI OIDC discovery failed: {err}")))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|err| AgentError::Http(err.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(AgentError::Http(format!(
                "xAI OIDC discovery returned HTTP {status}: {text}"
            )));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| AgentError::Parse(format!("xAI discovery invalid JSON: {err}")))?;
        let authorization_endpoint =
            required_str(&value, "authorization_endpoint", "xAI discovery")?;
        let token_endpoint = required_str(&value, "token_endpoint", "xAI discovery")?;
        validate_xai_endpoint(&authorization_endpoint, "authorization_endpoint")?;
        validate_xai_endpoint(&token_endpoint, "token_endpoint")?;
        Ok(XaiDiscovery {
            authorization_endpoint,
            token_endpoint,
        })
    }

    fn authorize_url(
        authorization_endpoint: &str,
        redirect_uri: &str,
        state: &str,
        pkce: &Pkce,
    ) -> AgentResult<String> {
        let nonce = oauth::random_urlsafe(24);
        let mut url = url::Url::parse(authorization_endpoint)
            .map_err(|err| AgentError::Auth(err.to_string()))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", XAI_CLIENT_ID)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", XAI_SCOPE)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state)
            .append_pair("nonce", &nonce)
            .append_pair("plan", "generic")
            .append_pair("referrer", "nerve");
        Ok(url.to_string())
    }

    fn credential_from_token(value: &Value) -> AgentResult<Credential> {
        let access_token = required_str(value, "access_token", "xAI token")?;
        let refresh_token = optional_str(value, "refresh_token");
        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or_else(|| AgentError::Parse("xAI token response missing `expires_in`".into()))?;
        Ok(Credential {
            provider: ProviderId::Xai,
            mode: AuthMode::Oauth,
            access_token,
            refresh_token,
            expires_at_unix: Some(oauth::expires_at(expires_in, EXPIRY_SKEW_SECS)),
            account_id: None,
            base_url: ProviderId::Xai.default_base_url().to_string(),
        })
    }
}

impl AuthStrategy for XaiAuth {
    fn provider(&self) -> ProviderId {
        ProviderId::Xai
    }

    fn default_redirect_uri(&self) -> String {
        default_redirect_uri(&Self::loopback())
    }

    fn start(&self, redirect_uri: &str) -> AgentResult<LoginStart> {
        let discovery = Self::discover()?;
        let authorize = discovery.authorization_endpoint.clone();
        let mut start =
            make_login_start(ProviderId::Xai, redirect_uri, |redirect, state, pkce| {
                Self::authorize_url(&authorize, redirect, state, pkce)
            })?;
        start.provider_data = json!({ "token_endpoint": discovery.token_endpoint });
        Ok(start)
    }

    fn complete(
        &self,
        start: &LoginStart,
        callback: &oauth::OAuthCallback,
        cancel: &nerve_core::CancelToken,
    ) -> AgentResult<Credential> {
        ensure_login_provider(start, ProviderId::Xai)?;
        let token_endpoint = start
            .provider_data
            .get("token_endpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| AgentError::Auth("xAI login state missing token_endpoint".into()))?;
        validate_xai_endpoint(token_endpoint, "token_endpoint")?;
        let code = validated_code(start, callback)?;
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", XAI_CLIENT_ID),
            ("code", code.as_str()),
            ("redirect_uri", start.redirect_uri.as_str()),
            ("code_verifier", start.verifier.as_str()),
        ];
        let value = post_token_form_cancel(token_endpoint, &form, cancel)?;
        let cred = Self::credential_from_token(&value)?;
        if cred.refresh_token.is_none() {
            return Err(AgentError::Auth(
                "xAI token exchange response missing refresh_token".into(),
            ));
        }
        Ok(cred)
    }

    fn login(&self, opts: &LoginOptions) -> AgentResult<Credential> {
        run_interactive_login(self, &Self::loopback(), opts)
    }

    fn refresh(&self, cred: &Credential) -> AgentResult<Credential> {
        let refresh_token = cred
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| AgentError::Auth("xAI credential has no refresh_token".into()))?;
        let discovery = Self::discover()?;
        validate_xai_endpoint(&discovery.token_endpoint, "token_endpoint")?;
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", XAI_CLIENT_ID),
            ("refresh_token", refresh_token),
        ];
        let value = post_token_form(&discovery.token_endpoint, &form)?;
        let mut refreshed = Self::credential_from_token(&value)?;
        if refreshed.refresh_token.is_none() {
            refreshed.refresh_token = Some(refresh_token.to_string());
        }
        Ok(refreshed)
    }
}

/// Pin an xAI OIDC endpoint to HTTPS on the `x.ai` origin (or a `*.x.ai`
/// subdomain), with no query string. Guards against a poisoned discovery doc
/// silently redirecting refresh tokens to a foreign host.
fn validate_xai_endpoint(endpoint: &str, field: &str) -> AgentResult<()> {
    let invalid = || AgentError::Auth(format!("invalid xAI {field}: {endpoint}"));
    let parsed = url::Url::parse(endpoint).map_err(|_| invalid())?;
    if parsed.scheme() != "https" {
        return Err(invalid());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid());
    }
    if parsed.query().is_some() {
        return Err(invalid());
    }
    let host = parsed
        .host_str()
        .map(str::to_ascii_lowercase)
        .ok_or_else(invalid)?;
    if host == "x.ai" || host.ends_with(".x.ai") {
        Ok(())
    } else {
        Err(invalid())
    }
}

#[cfg(test)]
#[path = "strategy_tests.rs"]
mod strategy_tests;
