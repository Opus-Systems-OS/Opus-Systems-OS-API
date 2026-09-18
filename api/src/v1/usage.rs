//! `/v1/usage` — spend rollups the control plane keeps from Anthropic's
//! webhooks: per agent, and the most recent sessions. `export.csv` is the
//! audit trail, oldest first.

use super::AppState;
use crate::error::{Error, Result};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    /// Inclusive lower bound on when the rollup was observed: RFC 3339, or
    /// `YYYY-MM-DD` for midnight UTC.
    pub since: Option<String>,
    /// Exclusive upper bound, same forms.
    pub until: Option<String>,
}

impl UsageQuery {
    fn pairs(&self) -> Vec<(&str, &str)> {
        let mut q = Vec::new();
        if let Some(s) = &self.since {
            q.push(("since", s.as_str()));
        }
        if let Some(u) = &self.until {
            q.push(("until", u.as_str()));
        }
        q
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AgentUsage {
    pub agent_slug: String,
    pub session_count: u64,
    pub total_list_cost_cents: u64,
    pub budget_reached_count: u64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SessionUsage {
    pub session_id: String,
    pub agent_slug: String,
    pub environment_slug: Option<String>,
    /// Whole US cents as a string; null until a webhook has reported it.
    pub list_cost_cents: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub active_seconds: Option<f64>,
    pub budget_reached: bool,
    pub last_event_type: String,
    pub observed_at: String,
    /// `"<type>: <message>"` when the session's last turn ended on a
    /// `session.error` (billing, upstream outage) instead of a reply.
    pub last_error: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct UsageWindow {
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Usage {
    pub window: UsageWindow,
    pub by_agent: Vec<AgentUsage>,
    /// Most recently observed first, at most 100.
    pub recent: Vec<SessionUsage>,
}

#[utoipa::path(get, path = "/usage", tag = "usage", security(("api_key" = ["usage:read"])),
    params(UsageQuery),
    responses(
        (status = 200, body = Usage),
        (status = 400, description = "bad since/until", body = crate::openapi::ErrorBody),
    ))]
pub async fn get(
    State(state): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Value>> {
    Ok(Json(state.control_plane.get("/usage", &q.pairs()).await?))
}

#[utoipa::path(get, path = "/usage/export.csv", tag = "usage", security(("api_key" = ["usage:read"])),
    params(UsageQuery),
    responses((status = 200, description = "RFC 4180 CSV, oldest first; header row `session_id,agent_slug,…,last_error`", content_type = "text/csv")))]
pub async fn export_csv(
    State(state): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Response> {
    let upstream = state
        .control_plane
        .open::<Value>(Method::GET, "/usage/export.csv", &q.pairs(), None)
        .await?;
    let disposition = upstream.headers().get(header::CONTENT_DISPOSITION).cloned();
    let mut res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8");
    if let Some(d) = disposition {
        res = res.header(header::CONTENT_DISPOSITION, d);
    }
    res.body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| Error::Config(format!("csv response: {e}")))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get))
        .routes(routes!(export_csv))
}
