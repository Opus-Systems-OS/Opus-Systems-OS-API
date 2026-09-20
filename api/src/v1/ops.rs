//! `/v1/ops` — the stack's services at a glance, read-only. Registered
//! only when at least one service token is configured. Each service is
//! polled at most every 30 s however many clients ask; a service that
//! cannot be reached is a `down` row with the reason, never a failed hub.

use super::AppState;
use crate::error::{Error, Result};
use crate::upstream::ops::Status;
use axum::extract::{Path, State};
use axum::Json;
use serde::Serialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Serialize, utoipa::ToSchema)]
pub struct Hub {
    /// The configured services, hub order, without detail.
    pub services: Vec<Status>,
}

#[utoipa::path(get, path = "/ops", tag = "ops", security(("api_key" = ["ops:read"])),
    responses((status = 200, body = Hub)))]
pub async fn hub(State(state): State<AppState>) -> Result<Json<Hub>> {
    let ops = state.ops.as_ref().as_ref().ok_or(Error::NotFound)?;
    Ok(Json(Hub {
        services: ops.summary().await,
    }))
}

#[utoipa::path(get, path = "/ops/{service}", tag = "ops", security(("api_key" = ["ops:read"])),
    params(("service" = String, Path, description = "`github`, `uptimerobot`, `droplet`, `docker`, `tailscale` or `cloudflare`")),
    responses(
        (status = 200, body = Status, description = "The service with its `detail` document"),
        (status = 404, body = crate::openapi::ErrorBody, description = "Unknown, or not configured on this API"),
    ))]
pub async fn service(
    State(state): State<AppState>,
    Path(service): Path<String>,
) -> Result<Json<Status>> {
    let ops = state.ops.as_ref().as_ref().ok_or(Error::NotFound)?;
    Ok(Json(ops.status(&service).await?))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(hub))
        .routes(routes!(service))
}
