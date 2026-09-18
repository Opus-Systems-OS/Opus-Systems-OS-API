//! `/v1/inference` — the rig's local models (Ollama over Tailscale, via the
//! control plane). Bodies are Ollama's own (`/api/chat`, `/api/embed`) and
//! pass through untouched after a shape check; responses are Ollama's.
//! Rig off → `503 rig_offline` with `Retry-After`. Nothing retries on
//! Claude — that is the whole failover story.

use super::AppState;
use crate::error::{Error, Result};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use reqwest::Method;
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

fn body_or_400(body: std::result::Result<Json<Value>, JsonRejection>) -> Result<Value> {
    body.map(|Json(b)| b)
        .map_err(|e| Error::InvalidRequest(e.body_text()))
}

/// The same shape check the control plane makes, so a bad body is rejected
/// here with our envelope rather than after a round trip.
fn validate(body: &Value, field: &str, ok: fn(&Value) -> bool, expected: &str) -> Result<()> {
    let obj = body
        .as_object()
        .ok_or_else(|| Error::InvalidRequest("body must be a JSON object".into()))?;
    match obj.get("model").and_then(Value::as_str) {
        Some(m) if !m.trim().is_empty() => {}
        _ => {
            return Err(Error::InvalidRequest(
                "`model` must be a non-empty string".into(),
            ))
        }
    }
    match obj.get(field) {
        Some(v) if ok(v) => Ok(()),
        _ => Err(Error::InvalidRequest(format!(
            "`{field}` must be {expected}"
        ))),
    }
}

#[utoipa::path(get, path = "/inference/models", tag = "inference", security(("api_key" = ["inference"])),
    responses(
        (status = 200, description = "Ollama's `/api/tags`: `{\"models\":[{\"name\",\"size\",…}]}`", body = Object),
        (status = 503, description = "rig offline", body = crate::openapi::ErrorBody),
    ))]
pub async fn models(State(state): State<AppState>) -> Result<Json<Value>> {
    Ok(Json(
        state.control_plane.get("/inference/models", &[]).await?,
    ))
}

#[utoipa::path(post, path = "/inference/chat", tag = "inference", security(("api_key" = ["inference"])),
    request_body(content = Object, description = "Ollama `/api/chat` body: `{model, messages, stream?, options?, …}`. Streams NDJSON unless `\"stream\": false`."),
    responses(
        (status = 200, description = "`application/x-ndjson` frames, or one JSON object with `stream:false`"),
        (status = 400, body = crate::openapi::ErrorBody),
        (status = 503, description = "rig offline", body = crate::openapi::ErrorBody),
    ))]
pub async fn chat(
    State(state): State<AppState>,
    body: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<Response> {
    let body = body_or_400(body)?;
    validate(&body, "messages", Value::is_array, "an array")?;
    let streaming = body["stream"].as_bool().unwrap_or(true);
    let upstream = state
        .control_plane
        .open(Method::POST, "/inference/chat", &[], Some(&body))
        .await?;
    tracing::info!(
        model = body["model"].as_str().unwrap_or(""),
        streaming,
        "inference chat"
    );
    let content_type = if streaming {
        "application/x-ndjson"
    } else {
        "application/json"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| Error::Config(format!("chat response: {e}")))
}

#[utoipa::path(post, path = "/inference/embeddings", tag = "inference", security(("api_key" = ["inference"])),
    request_body(content = Object, description = "Ollama `/api/embed` body: `{model, input}` where `input` is a string or an array of strings."),
    responses(
        (status = 200, description = "Ollama's response: `{\"embeddings\":[[…]], …}`", body = Object),
        (status = 400, body = crate::openapi::ErrorBody),
        (status = 503, description = "rig offline", body = crate::openapi::ErrorBody),
    ))]
pub async fn embeddings(
    State(state): State<AppState>,
    body: std::result::Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>> {
    let body = body_or_400(body)?;
    validate(
        &body,
        "input",
        |v| v.is_string() || v.is_array(),
        "a string or an array",
    )?;
    let (_, out) = state
        .control_plane
        .post("/inference/embeddings", &body)
        .await?;
    Ok(Json(out))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(models))
        .routes(routes!(chat))
        .routes(routes!(embeddings))
        .route_layer(DefaultBodyLimit::max(1 << 20))
}
