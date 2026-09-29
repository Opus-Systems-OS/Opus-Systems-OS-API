//! OAuth for the sources that need a person's consent: Google (Gmail,
//! Calendar, YouTube) and WHOOP. Authorization-code flow, one user.
//!
//! 1. `opus-api oauth start google` (on the host) stores a one-time state
//!    and prints the consent URL.
//! 2. The provider redirects to `{API_PUBLIC_URL}/v1/oauth/{provider}/callback`;
//!    the state is taken (once, ≤ 15 min old) and the code exchanged.
//! 3. Tokens live in the API's database. An access token is refreshed a
//!    minute before it expires; a refresh token the provider rotates (WHOOP
//!    does, on every use) is replaced as soon as it arrives.
//!
//! Tokens never leave this module except as a bearer on the provider's own
//! API.

use super::{source_error, Sources};
use crate::config::{OAuthClient, SourcesConfig};
use crate::db::Db;
use crate::error::{Error, Result};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Google,
    Whoop,
}

/// How long a consent URL stays usable.
pub const STATE_TTL_SECS: i64 = 15 * 60;

impl Provider {
    pub fn parse(s: &str) -> Option<Provider> {
        match s {
            "google" => Some(Provider::Google),
            "whoop" => Some(Provider::Whoop),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::Whoop => "whoop",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Google => "Google",
            Provider::Whoop => "WHOOP",
        }
    }

    fn authorize_endpoint(self) -> &'static str {
        match self {
            Provider::Google => "https://accounts.google.com/o/oauth2/v2/auth",
            Provider::Whoop => "https://api.prod.whoop.com/oauth/oauth2/auth",
        }
    }

    /// Read-only, and only what the briefing uses.
    pub fn scopes(self) -> &'static str {
        match self {
            Provider::Google => concat!(
                "https://www.googleapis.com/auth/gmail.readonly ",
                "https://www.googleapis.com/auth/calendar.readonly ",
                "https://www.googleapis.com/auth/yt-analytics.readonly ",
                "https://www.googleapis.com/auth/youtube.readonly"
            ),
            Provider::Whoop => "read:recovery read:sleep read:cycles offline",
        }
    }

    /// The sources this provider's token serves, for cache invalidation.
    pub fn sources(self) -> &'static [&'static str] {
        match self {
            Provider::Google => &["gmail", "calendar", "youtube"],
            Provider::Whoop => &["whoop"],
        }
    }

    pub fn client(self, cfg: &SourcesConfig) -> Option<&OAuthClient> {
        match self {
            Provider::Google => cfg.google.as_ref(),
            Provider::Whoop => cfg.whoop.as_ref(),
        }
    }

    fn token_url(self, s: &Sources) -> &str {
        match self {
            Provider::Google => &s.urls.google_token,
            Provider::Whoop => &s.urls.whoop_token,
        }
    }
}

pub fn redirect_uri(cfg: &SourcesConfig, p: Provider) -> String {
    format!("{}/v1/oauth/{}/callback", cfg.public_url, p.as_str())
}

/// Store a fresh state and return the consent URL (for the CLI).
pub fn start(db: &Db, cfg: &SourcesConfig, p: Provider) -> Result<String> {
    let client = p.client(cfg).ok_or_else(|| {
        Error::Config(format!(
            "{} is not configured: set its client id and secret in the environment",
            p.name()
        ))
    })?;
    let mut raw = [0u8; 24];
    getrandom::fill(&mut raw).expect("os randomness");
    let state: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    db.insert_oauth_state(&state, p.as_str())?;
    let mut url = reqwest::Url::parse(p.authorize_endpoint()).expect("static URL");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &client.client_id)
            .append_pair("redirect_uri", &redirect_uri(cfg, p))
            .append_pair("response_type", "code")
            .append_pair("scope", p.scopes())
            .append_pair("state", &state);
        if p == Provider::Google {
            // A refresh token every time, not only on first consent.
            q.append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
    }
    Ok(url.to_string())
}

/// The callback: take the state, exchange the code, store the tokens.
pub async fn finish(s: &Sources, p: Provider, code: &str, state: &str) -> Result<()> {
    let owner = s.db.take_oauth_state(state, STATE_TTL_SECS)?;
    if owner.as_deref() != Some(p.as_str()) {
        return Err(Error::InvalidRequest(
            "this consent link is unknown, used or expired: run `opus-api oauth start` again"
                .into(),
        ));
    }
    let client = p.client(&s.cfg).ok_or(Error::NotFound)?;
    let body = token_request(
        s,
        p,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri(&s.cfg, p)),
            ("client_id", &client.client_id),
            ("client_secret", &client.client_secret),
        ],
    )
    .await?;
    let refresh = body["refresh_token"].as_str().ok_or_else(|| {
        source_error(
            p.name(),
            "no refresh token in the answer (was `offline` access granted?)",
        )
    })?;
    store(s, p, &body, Some(refresh))?;
    s.forget(p.sources()).await;
    tracing::info!(provider = p.as_str(), "oauth connected");
    Ok(())
}

/// A usable access token, refreshed if it is within a minute of expiry.
/// `Err` says to run `oauth start` when there has never been consent.
pub async fn access_token(s: &Sources, p: Provider) -> Result<String> {
    let _one_at_a_time = s.refresh_lock.lock().await;
    let row = s.db.oauth_token(p.as_str())?.ok_or_else(|| {
        source_error(
            p.name(),
            format!("not connected — run `opus-api oauth start {}`", p.as_str()),
        )
    })?;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if let (Some(token), Some(exp)) = (&row.access_token, row.expires_at) {
        if exp - 60 > now {
            return Ok(token.clone());
        }
    }
    let client = p.client(&s.cfg).ok_or(Error::NotFound)?;
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", row.refresh_token.as_str()),
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.as_str()),
    ];
    if p == Provider::Whoop {
        form.push(("scope", "offline"));
    }
    let body = token_request(s, p, &form).await?;
    let token = store(s, p, &body, body["refresh_token"].as_str())?;
    Ok(token)
}

async fn token_request(s: &Sources, p: Provider, form: &[(&str, &str)]) -> Result<Value> {
    let res = s.http.post(p.token_url(s)).form(form).send().await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let why = body["error_description"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("rejected");
        // invalid_grant = consent revoked or the refresh token is dead.
        let hint = if body["error"] == "invalid_grant" {
            format!(" — run `opus-api oauth start {}` again", p.as_str())
        } else {
            String::new()
        };
        return Err(source_error(
            p.name(),
            format!("token {}: {why}{hint}", status.as_u16()),
        ));
    }
    Ok(body)
}

/// Save what a token endpoint returned; the access token back.
fn store(s: &Sources, p: Provider, body: &Value, refresh: Option<&str>) -> Result<String> {
    let access = body["access_token"]
        .as_str()
        .ok_or_else(|| source_error(p.name(), "no access token in the answer"))?;
    let expires_in = body["expires_in"].as_i64().unwrap_or(3600);
    let expires_at = time::OffsetDateTime::now_utc().unix_timestamp() + expires_in;
    s.db.store_oauth_token(p.as_str(), refresh, access, expires_at)?;
    Ok(access.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SourcesConfig {
        SourcesConfig {
            google: Some(OAuthClient {
                client_id: "cid.apps.googleusercontent.com".into(),
                client_secret: "sec".into(),
            }),
            ..SourcesConfig::default()
        }
    }

    #[test]
    fn consent_url_carries_the_state_scopes_and_offline_access() {
        let db = Db::in_memory().unwrap();
        let url = start(&db, &cfg(), Provider::Google).unwrap();
        let u = reqwest::Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(u.host_str(), Some("accounts.google.com"));
        assert_eq!(
            q["redirect_uri"],
            "https://api.opustower.dev/v1/oauth/google/callback"
        );
        assert_eq!(q["access_type"], "offline");
        assert!(q["scope"].contains("gmail.readonly"));
        assert!(!url.contains("sec"), "the secret never goes in a URL");
        // One use.
        assert_eq!(
            db.take_oauth_state(&q["state"], STATE_TTL_SECS)
                .unwrap()
                .as_deref(),
            Some("google")
        );
        assert_eq!(
            db.take_oauth_state(&q["state"], STATE_TTL_SECS).unwrap(),
            None
        );
        // Unconfigured provider.
        assert!(start(&db, &cfg(), Provider::Whoop).is_err());
    }
}
