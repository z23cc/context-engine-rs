use nerve_runtime::RuntimeCommand;
use serde_json::Value;

use super::super::Shell;
use super::super::state::Tone;

impl Shell {
    /// `/lease [provider] [--refresh]`: ask the trusted host for an OAuth access-token
    /// lease, but render only redacted metadata in the TUI transcript.
    pub(super) async fn cmd_lease(&mut self, rest: &str) {
        let args = parse_lease_args(rest, &self.state.provider);
        let command = RuntimeCommand::AuthLease {
            provider: args.provider.clone(),
            force_refresh: args.force_refresh,
            include_token: true,
        };
        self.state.note(format!(
            "requesting short-lived OAuth lease for {}{}…",
            args.provider,
            if args.force_refresh {
                " (forces broker refresh)"
            } else {
                ""
            }
        ));
        match self.client.run_job(command, None).await {
            Ok(result) => self
                .state
                .push_notice(Tone::Info, format_auth_lease(&result)),
            Err(err) => self.state.push_notice(Tone::Error, err.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LeaseArgs {
    provider: String,
    force_refresh: bool,
}

fn parse_lease_args(rest: &str, fallback_provider: &str) -> LeaseArgs {
    let mut provider = None;
    let mut force_refresh = false;
    for part in rest.split_whitespace() {
        match part {
            "--refresh" | "--force-refresh" => force_refresh = true,
            other if provider.is_none() => provider = Some(other.to_string()),
            _ => {}
        }
    }
    LeaseArgs {
        provider: provider.unwrap_or_else(|| fallback_provider.to_string()),
        force_refresh,
    }
}

fn format_auth_lease(result: &Value) -> String {
    let provider = str_field(result, "provider", "unknown");
    let status = str_field(result, "status", "unknown");
    let mode = str_field(result, "mode", "oauth");
    let account = str_field(result, "account_id", "unknown");
    let base_url = str_field(result, "base_url", "unknown");
    let expires = result
        .get("expires_at_unix")
        .and_then(Value::as_u64)
        .map_or_else(|| "unknown".to_string(), |value| value.to_string());
    let access_token_line = if result
        .get("access_token_included")
        .and_then(Value::as_bool)
        .unwrap_or(result.get("access_token").is_some())
    {
        "received, redacted"
    } else {
        "not returned"
    };
    let refresh_held = result
        .get("refresh_token_held_by_broker")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    format!(
        "auth lease: {provider} · {status} · mode={mode}\n  base URL: {base_url}\n  account: {account}\n  expires at Unix: {expires}\n  access token: {access_token_line}\n  refresh token: {}",
        if refresh_held {
            "held by broker"
        } else {
            "not returned"
        }
    )
}

fn str_field(value: &Value, key: &str, fallback: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_lease_args_defaults_to_current_provider() {
        assert_eq!(
            parse_lease_args("", "chatgpt"),
            LeaseArgs {
                provider: "chatgpt".into(),
                force_refresh: false,
            }
        );
        assert_eq!(
            parse_lease_args("xai --refresh", "chatgpt"),
            LeaseArgs {
                provider: "xai".into(),
                force_refresh: true,
            }
        );
        assert_eq!(
            parse_lease_args("--force-refresh", "openai"),
            LeaseArgs {
                provider: "openai".into(),
                force_refresh: true,
            }
        );
    }

    #[test]
    fn format_auth_lease_redacts_tokens() {
        let out = format_auth_lease(&json!({
            "provider": "openai",
            "status": "leased",
            "mode": "oauth",
            "base_url": "https://api.openai.com",
            "account_id": "acct_123",
            "expires_at_unix": 12345,
            "access_token": "access-secret",
            "access_token_included": true,
            "refresh_token_held_by_broker": true,
        }));
        assert!(out.contains("auth lease: openai"));
        assert!(out.contains("access token: received, redacted"));
        assert!(out.contains("refresh token: held by broker"));
        for forbidden in [
            "access-secret",
            "refresh-secret",
            "Bearer ",
            "access_token",
            "refresh_token",
            "{\"",
        ] {
            assert!(!out.contains(forbidden), "leaked secret-shaped text: {out}");
        }
    }
}
