//! `/v1/fleet` — the fleet as configured: agents, and later environments.
//! Read-only by design; the fleet is defined in Iron-Fleet's `agents/`,
//! never through this API.

use super::AppState;
use crate::error::Result;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// One registered agent, as the control plane last synced it.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Agent {
    pub slug: String,
    pub agent_id: String,
    pub agent_version: u32,
    /// Per-session hard cap, whole US cents as a string (`"500"` = $5.00).
    pub max_list_cost_cents: String,
    /// `low`, `medium`, `high`.
    pub effort: String,
    pub default_environment: String,
    pub synced_at: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AgentList {
    pub data: Vec<Agent>,
}

#[utoipa::path(get, path = "/fleet/agents", tag = "fleet", security(("api_key" = ["fleet:read"])),
    responses((status = 200, body = AgentList)))]
pub async fn agents(State(state): State<AppState>) -> Result<Json<Value>> {
    let agents = state.control_plane.get("/agents", &[]).await?;
    // The control plane answers a bare array; every list here is `{data}`.
    Ok(Json(serde_json::json!({ "data": agents })))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(agents))
}
