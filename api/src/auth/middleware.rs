//! Bearer → `Principal`. `authenticate` runs on every protected route and
//! attaches the key's identity as a request extension; `require(scope)` is a
//! per-router layer that turns a missing scope into 403. Both answer with
//! `Error` so the body is the standard envelope.

use crate::auth::keys::{self, Scope, ScopeSet};
use crate::auth::rate_limit::RateLimiter;
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
}

impl Principal {
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
}

pub async fn authenticate(
    State(auth): State<Arc<AuthState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, Error> {
    let presented = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    let (id, secret) = keys::parse(presented).ok_or(Error::Unauthorized)?;
    let row = auth.db.key(id)?.ok_or(Error::Unauthorized)?;
    if row.revoked_at.is_some() || !keys::verify(secret, &row.secret_sha256) {
        return Err(Error::Unauthorized);
    }
    if let Err(retry_after_secs) = auth.limiter.check(&row.id) {
        return Err(Error::RateLimited { retry_after_secs });
    }
    if let Err(e) = auth.db.touch_key(id) {
        tracing::warn!(key = id, error = %e, "could not record last_used_at");
    }
    tracing::Span::current().record("key", row.id.as_str());
    req.extensions_mut().insert(Principal {
        key_id: row.id.clone(),
        name: row.name.clone(),
        scopes: row.scope_set(),
    });
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
