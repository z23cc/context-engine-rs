use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::{Args, Subcommand, ValueEnum};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    thread::sleep,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PROVIDER_ID: &str = "xai-oauth";
const DEFAULT_BASE_URL: &str = "https://api.x.ai/v1";
const DISCOVERY_URL: &str = "https://auth.x.ai/.well-known/openid-configuration";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
const REDIRECT_HOST: &str = "127.0.0.1";
const REDIRECT_PORT: u16 = 56_121;
const REDIRECT_PATH: &str = "/callback";
const REFRESH_SKEW_SECONDS: u64 = 3_600;

#[derive(Debug, Args)]
pub(super) struct AuthArgs {
    #[command(subcommand)]
    command: AuthCommand,
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Sign in to xAI Grok OAuth (SuperGrok / Premium+) with browser PKCE.
    Login(LoginArgs),
    /// Show stored xAI OAuth status without printing secrets.
    Status(StatusArgs),
    /// Remove stored xAI OAuth credentials.
    Logout(LogoutArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum AuthProvider {
    /// xAI Grok OAuth via browser PKCE.
    Xai,
}

#[derive(Debug, Args)]
struct LoginArgs {
    /// Provider to login. Only `xai` is supported.
    #[arg(value_enum)]
    provider: Option<AuthProvider>,
    /// Start a new browser OAuth flow even if stored credentials exist.
    #[arg(long)]
    force: bool,
    /// Print the authorization URL but do not try to open a browser.
    #[arg(long = "no-browser")]
    no_browser: bool,
    /// Skip the local listener and paste the callback URL/code manually.
    #[arg(long = "manual-paste")]
    manual_paste: bool,
    /// OAuth login timeout in seconds.
    #[arg(long = "timeout", default_value_t = 120)]
    timeout_seconds: u64,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Refresh the token if it is expiring before printing status.
    #[arg(long)]
    refresh: bool,
}

#[derive(Debug, Args)]
struct LogoutArgs {
    /// Provider to logout. Only `xai` is supported.
    #[arg(value_enum)]
    provider: Option<AuthProvider>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthStore {
    #[serde(default)]
    providers: BTreeMap<String, XaiProviderState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct XaiProviderState {
    #[serde(default)]
    tokens: Option<XaiTokens>,
    #[serde(default)]
    discovery: Option<XaiDiscovery>,
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    auth_mode: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    last_refresh_unix: Option<u64>,
    #[serde(default)]
    last_auth_error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XaiTokens {
    access_token: String,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_in: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct XaiDiscovery {
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug)]
pub(crate) struct RuntimeCredentials {
    pub(crate) base_url: String,
    pub(crate) access_token: String,
    pub(crate) last_refresh_unix: Option<u64>,
}

mod callback;
mod http;
mod oauth;
mod store;
mod util;

use oauth::{refresh_tokens, run_loopback_login};
use store::{
    acquire_auth_lock, auth_file_path, load_store, load_xai_state, save_store, save_xai_state,
    xai_state_and_tokens,
};
use util::{
    access_token_is_expiring, expiry_label, jwt_expiry, now_unix, validate_inference_base_url,
};

pub(super) fn run(args: AuthArgs) -> Result<()> {
    match args.command {
        AuthCommand::Login(login_args) => login(login_args),
        AuthCommand::Status(status_args) => status(status_args),
        AuthCommand::Logout(logout_args) => logout(logout_args),
    }
}

fn login(args: LoginArgs) -> Result<()> {
    ensure_provider(args.provider);
    if !args.force && try_reuse_existing()? {
        return Ok(());
    }

    println!("Signing in to xAI Grok OAuth (SuperGrok / Premium+)...");
    println!("Auth state: {}", auth_file_path()?.display());
    println!();

    let credentials = run_loopback_login(&args)?;
    let base_url = credentials.base_url.clone();
    let state = XaiProviderState {
        tokens: Some(credentials.tokens),
        discovery: Some(credentials.discovery),
        redirect_uri: Some(credentials.redirect_uri),
        base_url: Some(base_url.clone()),
        auth_mode: Some("oauth_pkce".to_string()),
        source: Some("oauth-loopback".to_string()),
        last_refresh_unix: Some(now_unix()),
        last_auth_error: None,
    };
    save_xai_state(state)?;
    println!();
    println!("Login successful.");
    println!("provider: {PROVIDER_ID}");
    println!("base_url: {base_url}");
    Ok(())
}

fn status(args: StatusArgs) -> Result<()> {
    if args.refresh {
        let credentials = resolve_runtime_credentials(true)?;
        print_status_authenticated(
            &auth_file_path()?,
            &credentials.base_url,
            &credentials.access_token,
            credentials.last_refresh_unix,
        );
        return Ok(());
    }

    let path = auth_file_path()?;
    let state = load_xai_state()?;
    let Some(state) = state else {
        println!("provider: {PROVIDER_ID}");
        println!("auth: {}", path.display());
        println!("status: not_logged_in");
        return Ok(());
    };
    let Some(tokens) = state.tokens else {
        println!("provider: {PROVIDER_ID}");
        println!("auth: {}", path.display());
        println!("status: invalid");
        return Ok(());
    };
    let base_url = state
        .base_url
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    print_status_authenticated(
        &path,
        &base_url,
        &tokens.access_token,
        state.last_refresh_unix,
    );
    Ok(())
}

fn logout(args: LogoutArgs) -> Result<()> {
    ensure_provider(args.provider);
    let path = auth_file_path()?;
    let _lock = acquire_auth_lock(&path)?;
    let mut store = load_store(&path)?;
    if store.providers.remove(PROVIDER_ID).is_some() {
        save_store(&path, &store)?;
        println!("Removed xAI OAuth credentials from {}", path.display());
    } else {
        println!("No xAI OAuth credentials found at {}", path.display());
    }
    Ok(())
}

fn ensure_provider(provider: Option<AuthProvider>) {
    let _ = provider.unwrap_or(AuthProvider::Xai);
}

fn try_reuse_existing() -> Result<bool> {
    let Some(state) = load_xai_state()? else {
        return Ok(false);
    };
    let Some(tokens) = state.tokens else {
        return Ok(false);
    };
    if access_token_is_expiring(&tokens.access_token, 60) {
        match resolve_runtime_credentials(true) {
            Ok(credentials) => {
                println!("Existing xAI OAuth credentials refreshed.");
                println!("provider: {PROVIDER_ID}");
                println!("base_url: {}", credentials.base_url);
                return Ok(true);
            }
            Err(err) => {
                eprintln!("Stored xAI OAuth credentials could not be refreshed: {err}");
                eprintln!("Starting a new login flow. Use --force to skip reuse checks.");
                return Ok(false);
            }
        }
    }
    println!("Existing xAI OAuth credentials found.");
    println!("Use `ctx-mcp auth login xai --force` to sign in again.");
    Ok(true)
}

fn print_status_authenticated(
    path: &Path,
    base_url: &str,
    access_token: &str,
    last_refresh: Option<u64>,
) {
    println!("provider: {PROVIDER_ID}");
    println!("auth: {}", path.display());
    println!("status: authenticated");
    println!("base_url: {base_url}");
    match jwt_expiry(access_token) {
        Some(exp) => println!(
            "access_token: present (expires_unix: {exp}, {})",
            expiry_label(exp)
        ),
        None => println!("access_token: present"),
    }
    println!("refresh_token: present");
    if let Some(value) = last_refresh {
        println!("last_refresh_unix: {value}");
    }
}

pub(crate) fn resolve_runtime_credentials(force_refresh: bool) -> Result<RuntimeCredentials> {
    let path = auth_file_path()?;
    let store = load_store(&path)?;
    let (mut state, mut tokens) = xai_state_and_tokens(&store)?;
    let needs_refresh =
        force_refresh || access_token_is_expiring(&tokens.access_token, REFRESH_SKEW_SECONDS);
    if needs_refresh {
        let _lock = acquire_auth_lock(&path)?;
        let mut store = load_store(&path)?;
        (state, tokens) = xai_state_and_tokens(&store)?;
        if force_refresh || access_token_is_expiring(&tokens.access_token, REFRESH_SKEW_SECONDS) {
            tokens = refresh_tokens(&state, &tokens)?;
            state.tokens = Some(tokens.clone());
            state.last_refresh_unix = Some(now_unix());
            state.last_auth_error = None;
            store
                .providers
                .insert(PROVIDER_ID.to_string(), state.clone());
            save_store(&path, &store)?;
        }
    }
    let base_url = validate_inference_base_url(state.base_url.as_deref())?;
    Ok(RuntimeCredentials {
        base_url,
        access_token: tokens.access_token,
        last_refresh_unix: state.last_refresh_unix,
    })
}

#[cfg(test)]
mod tests {
    use super::callback::parse_callback_target;
    use super::util::{pkce_challenge, validate_oauth_endpoint};
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn query_parser_decodes_callback_values() {
        let callback = parse_callback_target(
            "/callback?code=a%2Fb%2Bc&state=hello+world&error_description=nope",
            false,
        )
        .expect("callback");
        assert_eq!(callback.code.as_deref(), Some("a/b+c"));
        assert_eq!(callback.state.as_deref(), Some("hello world"));
        assert_eq!(callback.error_description.as_deref(), Some("nope"));
    }

    #[test]
    fn validates_xai_hosts_only() {
        validate_oauth_endpoint("https://auth.x.ai/token", "token_endpoint").expect("xai host");
        validate_oauth_endpoint("https://accounts.x.ai/oauth", "authorization_endpoint")
            .expect("xai subdomain");
        assert!(validate_oauth_endpoint("https://example.com/token", "token_endpoint").is_err());
        assert!(
            validate_oauth_endpoint("https://attacker.example?@api.x.ai/v1", "token_endpoint")
                .is_err()
        );
        assert!(validate_inference_base_url(Some("https://api.x.ai/v1")).is_ok());
        assert!(validate_inference_base_url(Some("https://staging.x.ai/v1")).is_ok());
        assert!(validate_inference_base_url(Some("https://x.ai/v1")).is_err());
        assert!(validate_inference_base_url(Some("http://api.x.ai/v1")).is_err());
        assert!(validate_inference_base_url(Some("https://api.x.ai/v1?token=leak")).is_err());
    }

    #[test]
    fn jwt_expiry_reads_exp_claim() {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":4102444800}"#);
        let token = format!("{header}.{payload}.sig");
        assert_eq!(jwt_expiry(&token), Some(4_102_444_800));
    }

    #[test]
    fn save_and_load_store_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        store.providers.insert(
            PROVIDER_ID.to_string(),
            XaiProviderState {
                tokens: Some(XaiTokens {
                    access_token: "access".to_string(),
                    refresh_token: "refresh".to_string(),
                    id_token: None,
                    expires_in: Some(3600),
                    token_type: Some("Bearer".to_string()),
                }),
                base_url: Some(DEFAULT_BASE_URL.to_string()),
                ..XaiProviderState::default()
            },
        );
        save_store(&path, &store).expect("save");
        let loaded = load_store(&path).expect("load");
        assert!(loaded.providers.contains_key(PROVIDER_ID));
    }
}
