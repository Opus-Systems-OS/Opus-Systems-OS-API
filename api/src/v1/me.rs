//! `GET /v1/me` — what the presented key is. The cheapest authenticated call:
//! a client's first request after configuration, and the exit test for
//! stage 1.

use super::AppState;
use crate::auth::keys::Scope;
use crate::auth::middleware::Principal;
use crate::error::Result;
use axum::extract::{Extension, State};
use axum::Json;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Serialize, utoipa::ToSchema)]
pub struct Me {
    /// The key's public id (the `osk_<id>_…` part).
    pub key_id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    pub created_at: String,
}

#[utoipa::path(get, path = "/me", tag = "auth", security(("api_key" = [])),
    responses(
        (status = 200, body = Me),
        (status = 401, description = "missing, malformed, unknown or revoked key", body = crate::openapi::ErrorBody),
    ))]
pub async fn me(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Me>> {
    let row = state
        .db
        .key(&principal.key_id)?
        .ok_or(crate::error::Error::Unauthorized)?;
    Ok(Json(Me {
        key_id: row.id,
        name: row.name,
        scopes: principal.scopes.iter().collect(),
        created_at: row.created_at,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(me))
}
