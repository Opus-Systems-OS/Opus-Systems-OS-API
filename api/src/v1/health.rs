//! `GET /v1/health` — public, no auth, no upstream call: "the API process is
//! up". Whether the fleet behind it is reachable is a separate, authenticated
//! question (`/v1/rig`, `/v1/fleet/agents`).

use super::AppState;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Serialize, utoipa::ToSchema)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
}

#[utoipa::path(get, path = "/health", tag = "health",
    responses((status = 200, body = Health)))]
pub async fn health() -> axum::Json<Health> {
    axum::Json(Health {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(health))
}
