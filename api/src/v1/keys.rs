//! `/v1/keys` — key administration, `keys:admin` only. The plaintext is
//! returned exactly once, from `create`; nothing can show it again.

use super::AppState;
use crate::auth::keys::{self, Scope};
use crate::db::{self, KeyRow};
use crate::error::{Error, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateKey {
    /// Who holds it: "mac", "quest-3", "jarvis-ios". 1–64 chars.
    pub name: String,
    /// At least one. `keys:admin` cannot be granted over HTTP — only the
    /// CLI on the box mints admin keys.
    pub scopes: Vec<Scope>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct CreatedKey {
    pub id: String,
    pub name: String,
    pub scopes: Vec<Scope>,
    pub created_at: String,
    /// Shown once. Store it; it cannot be retrieved.
    pub key: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct KeyList {
    pub data: Vec<KeyRow>,
}

/// Validates and stores a new key; shared with the CLI, which is the only
/// path allowed to include `keys:admin`.
pub fn create_key(db: &db::Db, name: &str, scopes: &[Scope]) -> Result<CreatedKey> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(Error::InvalidRequest("name must be 1–64 characters".into()));
    }
    if scopes.is_empty() {
        return Err(Error::InvalidRequest("at least one scope".into()));
    }
    let scopes: Vec<Scope> = {
        let mut s = scopes.to_vec();
        s.sort();
        s.dedup();
        s
    };
    let minted = keys::mint();
    let row = KeyRow {
        id: minted.id.clone(),
        name: name.to_owned(),
        secret_sha256: minted.secret_sha256,
        scopes: scopes.clone(),
        created_at: db::now(),
        last_used_at: None,
        revoked_at: None,
    };
    db.insert_key(&row)?;
    Ok(CreatedKey {
        id: row.id,
        name: row.name,
        scopes,
        created_at: row.created_at,
        key: minted.plaintext,
    })
}

#[utoipa::path(post, path = "/keys", tag = "keys", security(("api_key" = ["keys:admin"])),
    request_body = CreateKey,
    responses(
        (status = 201, body = CreatedKey),
        (status = 400, body = crate::openapi::ErrorBody),
        (status = 403, body = crate::openapi::ErrorBody),
    ))]
pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateKey>,
) -> Result<(StatusCode, Json<CreatedKey>)> {
    if body.scopes.contains(&Scope::KeysAdmin) {
        return Err(Error::InvalidRequest(
            "keys:admin can only be minted with `opus-api keys create` on the host".into(),
        ));
    }
    let created = create_key(&state.db, &body.name, &body.scopes)?;
    tracing::info!(key = %created.id, name = %created.name, scopes = ?created.scopes, "key created");
    Ok((StatusCode::CREATED, Json(created)))
}

#[utoipa::path(get, path = "/keys", tag = "keys", security(("api_key" = ["keys:admin"])),
    responses((status = 200, body = KeyList)))]
pub async fn list(State(state): State<AppState>) -> Result<Json<KeyList>> {
    Ok(Json(KeyList {
        data: state.db.keys()?,
    }))
}

#[utoipa::path(delete, path = "/keys/{id}", tag = "keys", security(("api_key" = ["keys:admin"])),
    params(("id" = String, Path, description = "the key's public id")),
    responses(
        (status = 204, description = "revoked"),
        (status = 404, description = "no such key, or already revoked", body = crate::openapi::ErrorBody),
    ))]
pub async fn revoke(State(state): State<AppState>, Path(id): Path<String>) -> Result<StatusCode> {
    if state.db.revoke_key(&id)? {
        tracing::info!(key = %id, "key revoked");
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound)
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(revoke))
}
