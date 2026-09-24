//! `/v1/clients` — which devices have used the API, and when. Names and
//! times only: a client key's id, secret and scopes never leave `/v1/keys`
//! (admin-only). Several keys can share a name — a headset re-pairing mints
//! a new `quest-3` and revokes the old — so each name appears once, with the
//! latest use among its live keys. Revoked keys don't count.

use super::AppState;
use crate::error::Result;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use std::collections::BTreeMap;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Serialize, utoipa::ToSchema)]
pub struct Client {
    /// The key's human name: `mac`, `quest-3`, `web`, `win-rig`.
    pub name: String,
    /// Last authenticated request (RFC 3339; recorded at most about once a
    /// minute). `null` if a live key of this name has never been used.
    pub last_seen_at: Option<String>,
    /// When the newest live key of this name was minted (or paired).
    pub since: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Clients {
    pub data: Vec<Client>,
}

#[utoipa::path(get, path = "/clients", tag = "ops", security(("api_key" = ["ops:read"])),
    responses((status = 200, body = Clients)))]
pub async fn list(State(state): State<AppState>) -> Result<Json<Clients>> {
    let mut by_name: BTreeMap<String, Client> = BTreeMap::new();
    for k in state
        .db
        .keys()?
        .into_iter()
        .filter(|k| k.revoked_at.is_none())
    {
        let entry = by_name.entry(k.name.clone()).or_insert_with(|| Client {
            name: k.name.clone(),
            last_seen_at: None,
            since: k.created_at.clone(),
        });
        // RFC 3339 strings from `db::now()` sort chronologically.
        if k.last_used_at > entry.last_seen_at {
            entry.last_seen_at = k.last_used_at.clone();
        }
        if k.created_at > entry.since {
            entry.since = k.created_at.clone();
        }
    }
    Ok(Json(Clients {
        data: by_name.into_values().collect(),
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list))
}
