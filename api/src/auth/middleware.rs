//! Bearer → `Principal`. `authenticate` runs on every protected route and
//! attaches the key's identity as a request extension; `require(scope)` is a
//! per-router layer that turns a missing scope into 403. Both answer with
//! `Error` so the body is the standard envelope.

use crate::auth::keys::{self, Scope, ScopeSet};
use crate::auth::rate_limit::{peer, RateLimiter};
use crate::db::Db;
use crate::error::Error;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Principal {
    pub key_id: String,
    pub name: String,
    pub scopes: ScopeSet,
    /// The agents this key is limited to; `None` = every agent.
    pub agents: Option<Vec<String>>,
}

impl Principal {
    /// Whether this key may reach `slug`'s sessions.
    pub fn may_reach(&self, slug: &str) -> bool {
        self.agents
            .as_ref()
            .is_none_or(|list| list.iter().any(|a| a == slug))
    }

    pub fn allow_agent(&self, slug: &str) -> Result<(), Error> {
        if self.may_reach(slug) {
            Ok(())
        } else {
            Err(Error::AgentNotAllowed(slug.to_owned()))
        }
    }

    pub fn require(&self, scope: Scope) -> Result<(), Error> {
        if self.scopes.has(scope) {
            Ok(())
        } else {
            Err(Error::Forbidden(scope.as_str()))
        }
    }
}

pub struct AuthState {
    pub db: Db,
    pub limiter: Arc<RateLimiter>,
    /// Failed authentications per peer address; an address over it is
    /// refused before its key is even looked up.
    pub failures: Arc<RateLimiter>,
}

pub async fn authenticate(
    State(auth): State<Arc<AuthState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, Error> {
    let who = peer(req.headers());
    if let Err(retry_after_secs) = auth.failures.blocked(&who) {
        return Err(Error::RateLimited { retry_after_secs });
    }
    let presented = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let row = match keys::parse(presented) {
        Some((id, secret)) => auth
            .db
            .key(id)?
            .filter(|row| row.revoked_at.is_none() && keys::verify(secret, &row.secret_sha256)),
        None => None,
    };
    let Some(row) = row else {
        let _ = auth.failures.check(&who); // spend one failure
        return Err(Error::Unauthorized);
    };
    if let Err(retry_after_secs) = auth.limiter.check(&row.id) {
        return Err(Error::RateLimited { retry_after_secs });
    }
    let id = row.id.as_str();
    if let Err(e) = auth.db.touch_key(id) {
        tracing::warn!(key = id, error = %e, "could not record last_used_at");
    }
    tracing::Span::current().record("key", row.id.as_str());
    req.extensions_mut().insert(Principal {
        key_id: row.id.clone(),
        name: row.name.clone(),
        scopes: row.scope_set(),
        agents: row.agents.clone(),
    });
    Ok(next.run(req).await)
}

/// A second, smaller per-key bucket for routes that cost money or the GPU:
/// `route_layer(from_fn_with_state(limiter, costly))`, inside `authenticate`.
pub async fn costly(
    State(limiter): State<Arc<RateLimiter>>,
    req: Request,
    next: Next,
) -> Result<Response, Error> {
    let principal = req
        .extensions()
        .get::<Principal>()
        .ok_or(Error::Unauthorized)?;
    limiter
        .check(&principal.key_id)
        .map_err(|retry_after_secs| Error::RateLimited { retry_after_secs })?;
    Ok(next.run(req).await)
}

/// `Router::route_layer(axum::middleware::from_fn_with_state(scope, require))`.
pub async fn require(
    State(scope): State<Scope>,
    req: Request,
    next: Next,
) -> Result<Response, Error> {
    let principal = req
        .extensions()
        .get::<Principal>()
        .ok_or(Error::Unauthorized)?;
    principal.require(scope)?;
    Ok(next.run(req).await)
}
