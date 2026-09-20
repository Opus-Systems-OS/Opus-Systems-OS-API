//! `/v1/pair` — pairing a new device without ever typing a key into it.
//!
//! The device (a headset with no key) starts a pairing and shows the
//! six-digit **code**; it also receives a **token** it keeps to itself. A
//! human types the code into a client that already holds a key with
//! `pair:approve` (the Mac's Jarvis app). Approval mints a key with the
//! *fixed device profile* — never more, never `keys:admin` — revokes older
//! keys of the same name, and parks the key with the extras the device
//! needs (Wit token, the Mac's address) under the code. The device polls
//! with its token and collects the bundle exactly once.
//!
//! Codes live ten minutes, in memory (one API instance). A guessed code
//! yields nothing without the token; approving needs a real key.

use super::AppState;
use crate::auth::keys::Scope;
use crate::auth::rate_limit::RateLimiter;
use crate::error::{Error, Result};
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub const TTL: Duration = Duration::from_secs(600);
/// Pairings a single address may start per minute.
const STARTS_PER_MINUTE: u32 = 10;

/// What every paired device gets, and nothing else.
pub const DEVICE_PROFILE: [Scope; 5] = [
    Scope::FleetRead,
    Scope::SessionsRead,
    Scope::SessionsWrite,
    Scope::Voice,
    Scope::OpsRead,
];

#[derive(Clone)]
pub struct Pairings {
    inner: Arc<Mutex<HashMap<String, Pending>>>,
    starts: Arc<RateLimiter>,
}

struct Pending {
    token: String,
    started: Instant,
    bundle: Option<Bundle>,
}

#[derive(Clone, Serialize, utoipa::ToSchema)]
pub struct Bundle {
    /// The device's new key — `osk_…`. Shown once.
    pub api_key: String,
    pub key_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wit_token: Option<String>,
    /// `host:port` of the Mac's music stream, when the approver has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

impl Default for Pairings {
    fn default() -> Self {
        Pairings {
            inner: Arc::new(Mutex::new(HashMap::new())),
            starts: Arc::new(RateLimiter::new(STARTS_PER_MINUTE)),
        }
    }
}

impl Pairings {
    fn sweep(map: &mut HashMap<String, Pending>) {
        map.retain(|_, p| p.started.elapsed() < TTL);
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Started {
    /// Six digits, for a human to type into the approving client.
    pub code: String,
    /// The device keeps this and presents it when collecting; a code alone collects nothing.
    pub token: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Approve {
    /// The device's key name, e.g. `quest-3`. Older keys with this name are revoked.
    pub name: String,
    /// Wit.ai client token for the device's dictation, passed through untouched.
    #[serde(default)]
    pub wit_token: Option<String>,
    /// `host:port` of the approving Mac's music stream.
    #[serde(default)]
    pub speaker: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Approved {
    pub key_id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct Claim {
    pub token: String,
}

/// The caller's address for the start limiter: what the proxy says, else
/// "unknown" (all direct callers share one bucket — only tests).
fn peer(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn random_code() -> String {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("os randomness");
    format!("{:06}", u32::from_le_bytes(b) % 1_000_000)
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("os randomness");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[utoipa::path(post, path = "/pair", tag = "pair",
    responses(
        (status = 201, body = Started, description = "Show `code` to the human; keep `token`"),
        (status = 429, body = crate::openapi::ErrorBody),
    ))]
pub async fn start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<Started>)> {
    let who = peer(&headers);
    state
        .pairings
        .starts
        .check(&who)
        .map_err(|retry_after_secs| Error::RateLimited { retry_after_secs })?;
    let mut map = state.pairings.inner.lock().unwrap();
    Pairings::sweep(&mut map);
    let mut code = random_code();
    while map.contains_key(&code) {
        code = random_code();
    }
    let token = random_token();
    map.insert(
        code.clone(),
        Pending {
            token: token.clone(),
            started: Instant::now(),
            bundle: None,
        },
    );
    tracing::info!(code = %code, from = %who, "pairing started");
    Ok((
        StatusCode::CREATED,
        Json(Started {
            code,
            token,
            expires_in: TTL.as_secs(),
        }),
    ))
}

#[utoipa::path(post, path = "/pair/{code}/approve", tag = "pair", security(("api_key" = ["pair:approve"])),
    params(("code" = String, Path, description = "the six digits the device shows")),
    request_body = Approve,
    responses(
        (status = 200, body = Approved, description = "A device key was minted with the fixed device profile"),
        (status = 404, body = crate::openapi::ErrorBody, description = "No such pending code (expired, mistyped, or already approved)"),
    ))]
pub async fn approve(
    State(state): State<AppState>,
    Path(code): Path<String>,
    body: std::result::Result<Json<Approve>, JsonRejection>,
) -> Result<Json<Approved>> {
    let Json(body) = body.map_err(|e| Error::InvalidRequest(e.body_text()))?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(Error::InvalidRequest("name must be 1-64 characters".into()));
    }
    if let Some(s) = &body.speaker {
        if s.len() > 64
            || !s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-'))
        {
            return Err(Error::InvalidRequest("speaker must be host:port".into()));
        }
    }
    {
        let mut map = state.pairings.inner.lock().unwrap();
        Pairings::sweep(&mut map);
        match map.get(&code) {
            Some(p) if p.bundle.is_none() => {}
            _ => return Err(Error::NotFound),
        }
    }
    // Rotation is built in: the name identifies the device.
    for old in state.db.keys()? {
        if old.name == name && old.revoked_at.is_none() {
            state.db.revoke_key(&old.id)?;
            tracing::info!(key = %old.id, name = %name, "older key revoked by pairing");
        }
    }
    let created = super::keys::create_key(&state.db, name, &DEVICE_PROFILE)?;
    let mut map = state.pairings.inner.lock().unwrap();
    let Some(p) = map.get_mut(&code) else {
        return Err(Error::NotFound);
    };
    p.bundle = Some(Bundle {
        api_key: created.key,
        key_id: created.id.clone(),
        name: created.name.clone(),
        wit_token: body.wit_token.filter(|t| !t.is_empty()),
        speaker: body.speaker.filter(|s| !s.is_empty()),
    });
    tracing::info!(code = %code, key = %created.id, name = %created.name, "pairing approved");
    Ok(Json(Approved {
        key_id: created.id,
        name: created.name,
        scopes: DEVICE_PROFILE.to_vec(),
    }))
}

#[utoipa::path(get, path = "/pair/{code}", tag = "pair",
    params(("code" = String, Path, description = "the code from `POST /pair`"), Claim),
    responses(
        (status = 200, body = Bundle, description = "Approved: the key and extras, once"),
        (status = 202, description = "Not approved yet — poll again"),
        (status = 404, body = crate::openapi::ErrorBody, description = "Unknown or expired code, or wrong token"),
    ))]
pub async fn claim(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Query(q): Query<Claim>,
) -> Result<Response> {
    let mut map = state.pairings.inner.lock().unwrap();
    Pairings::sweep(&mut map);
    let Some(p) = map.get(&code) else {
        return Err(Error::NotFound);
    };
    if !constant_eq(&p.token, &q.token) {
        return Err(Error::NotFound);
    }
    if p.bundle.is_none() {
        return Ok(StatusCode::ACCEPTED.into_response());
    }
    let p = map.remove(&code).expect("present");
    tracing::info!(code = %code, "pairing claimed");
    Ok(Json(p.bundle.expect("approved")).into_response())
}

fn constant_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// The two unauthenticated routes (start, claim).
pub fn open_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(start))
        .routes(routes!(claim))
}

/// The approving route (needs `pair:approve`).
pub fn approve_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(approve))
}
