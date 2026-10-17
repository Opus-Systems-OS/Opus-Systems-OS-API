//! `/v1/intel/*` — open-source news and hazards for the globe: wires,
//! quakes, fires, storms and alerts, each categorized and (where the words
//! allow) placed. Read-only, public sources only, behind `intel:read`.

use super::AppState;
use crate::error::{Error, Result};
use crate::upstream::intel::{clean_query, Feed, SearchResult, MAX_QUERY};
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

fn json(body: Bytes) -> Response {
    let mut res = body.into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    res
}

#[utoipa::path(get, path = "/intel/feed", tag = "intel", security(("api_key" = ["intel:read"])),
    responses((status = 200, body = Feed, description = "Every source at once, cached five minutes; a failing source is a `sources[]` row with `ok: false`")))]
pub async fn feed(State(state): State<AppState>) -> Response {
    json(state.intel.feed().await)
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct SearchParams {
    /// A name, place or event; 1–80 characters.
    #[serde(default)]
    q: String,
}

#[utoipa::path(get, path = "/intel/search", tag = "intel", security(("api_key" = ["intel:read"])),
    params(SearchParams),
    responses(
        (status = 200, body = SearchResult, description = "News coverage and Wikipedia entries, cached ten minutes per query"),
        (status = 400, body = crate::openapi::ErrorBody, description = "`q` missing or over 80 characters"),
    ))]
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Response> {
    let q = clean_query(&params.q)
        .ok_or_else(|| Error::InvalidRequest(format!("q must be 1–{MAX_QUERY} characters")))?;
    Ok(json(state.intel.search(&q).await?))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(feed))
        .routes(routes!(search))
}
