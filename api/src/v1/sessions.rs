//! `/v1/sessions` — start, list, inspect, message, stream, interrupt.
//! Reads need `sessions:read`, writes `sessions:write`. `GET` and `POST`
//! share paths here, so the scope is checked per handler from the
//! `Principal` extension rather than by a router-wide layer.
//!
//! Session and event objects are Anthropic's Managed Agents objects, passed
//! through unchanged (plus `console_url`, which the control plane adds).
//! They are documented here as open objects: their shape is Anthropic's
//! contract, versioned by the beta header the control plane sends, and
//! re-typing them would only add a place for drift.

use super::AppState;
use crate::auth::keys::Scope;
use crate::auth::middleware::Principal;
use crate::error::{Error, Result};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// Session and agent ids are path segments upstream; keep them to the
/// charset Anthropic uses so nothing else can be spliced into a URL.
pub(super) fn valid_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidRequest(
            "session id has unexpected characters".into(),
        ))
    }
}

fn body_or_400<T>(body: std::result::Result<Json<T>, JsonRejection>) -> Result<T> {
    body.map(|Json(b)| b)
        .map_err(|e| Error::InvalidRequest(e.body_text()))
}

// ---- start --------------------------------------------------------------

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateSession {
    /// A fleet agent: `jarvis`, `blueweb-client`, `blueweb-ops`, `gpu-compute`.
    pub agent_slug: String,
    /// The first user message. Its first line becomes the session title.
    pub task: String,
    /// Override the agent's default environment (`cloud-default`, `rig-gpu`,
    /// `blueweb-web`). Sessions on `rig-gpu` queue while the rig is off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Extra `https://github.com/<owner>/<repo>` URLs to mount, on top of
    /// the agent's own. Only agents with a GitHub credential accept these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<String>,
    /// Tools *this client* executes, declared for this session only (the
    /// agent is untouched; other clients' sessions never see them). When
    /// the agent calls one, the session emits `agent.custom_tool_use` and
    /// idles with `stop_reason.type = "requires_action"` until the client
    /// answers via `POST /sessions/{id}/tool-results` (or the WebSocket
    /// `tool_result` frame).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<CustomTool>,
    /// Appended to the agent's system prompt for this session only, after
    /// a blank line — a client's personality or device context. ≤ 4000 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_suffix: Option<String>,
    /// Which client started the session — `quest`, `mac`, `tauri` — kept in
    /// the session's metadata as `iron_fleet_client`, so another device can
    /// find it (the Mac answers the headset's music tools this way).
    /// `[a-z0-9-]`, ≤ 32 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// A client-executed tool. `input_schema` is a JSON Schema object.
#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomTool {
    /// Always `custom`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `[a-z0-9_]`, 1–64 chars, unique within the session.
    pub name: String,
    /// What it does and when to use it — the model reads this.
    pub description: String,
    #[schema(value_type = Object)]
    pub input_schema: Value,
}

/// Answers to `agent.custom_tool_use` events.
#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolResults {
    pub results: Vec<ToolResult>,
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    /// The `id` of the `agent.custom_tool_use` event being answered.
    pub custom_tool_use_id: String,
    /// What the tool returned, as text.
    pub content: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
}

fn validate_custom_tools(tools: &[CustomTool]) -> Result<()> {
    if tools.len() > 32 {
        return Err(Error::InvalidRequest("at most 32 custom tools".into()));
    }
    let mut seen = std::collections::HashSet::new();
    for t in tools {
        if t.kind != "custom" {
            return Err(Error::InvalidRequest(format!(
                "tool `{}`: type must be `custom`",
                t.name
            )));
        }
        let ok = !t.name.is_empty()
            && t.name.len() <= 64
            && t.name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Err(Error::InvalidRequest(format!(
                "tool name `{}` must be 1-64 chars of [a-z0-9_]",
                t.name
            )));
        }
        if !seen.insert(t.name.as_str()) {
            return Err(Error::InvalidRequest(format!(
                "duplicate tool `{}`",
                t.name
            )));
        }
        if t.description.trim().is_empty() {
            return Err(Error::InvalidRequest(format!(
                "tool `{}` needs a description",
                t.name
            )));
        }
        if !t.input_schema.is_object() {
            return Err(Error::InvalidRequest(format!(
                "tool `{}`: input_schema must be an object",
                t.name
            )));
        }
    }
    Ok(())
}

/// What `POST /v1/sessions` returns: the new session's identity, where it
/// runs, and its hard spend cap in whole US cents (a string, e.g. `"500"`).
#[derive(Serialize, utoipa::ToSchema)]
pub struct CreatedSession {
    pub session_id: String,
    pub status: String,
    pub agent_slug: String,
    pub agent_id: String,
    pub agent_version: u32,
    pub environment: String,
    pub environment_id: String,
    pub budget: BudgetSummary,
    pub console_url: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct BudgetSummary {
    /// Whole US cents as a string, e.g. `"500"` = $5.00.
    pub max_list_cost_cents: String,
}

#[utoipa::path(post, path = "/sessions", tag = "sessions", security(("api_key" = ["sessions:write"])),
    request_body = CreateSession,
    responses(
        (status = 201, body = CreatedSession),
        (status = 400, description = "empty task, bad body", body = crate::openapi::ErrorBody),
        (status = 404, description = "unknown agent or environment", body = crate::openapi::ErrorBody),
        (status = 409, description = "environment defined but not provisioned", body = crate::openapi::ErrorBody),
    ))]
pub async fn create(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    body: std::result::Result<Json<CreateSession>, JsonRejection>,
) -> Result<(StatusCode, Json<Value>)> {
    who.require(Scope::SessionsWrite)?;
    let req = body_or_400(body)?;
    if req.task.trim().is_empty() {
        return Err(Error::InvalidRequest("task must not be empty".into()));
    }
    valid_id(&req.agent_slug)
        .map_err(|_| Error::InvalidRequest("agent_slug has unexpected characters".into()))?;
    validate_custom_tools(&req.tools)?;
    if let Some(sfx) = &req.system_suffix {
        if sfx.chars().count() > 4_000 {
            return Err(Error::InvalidRequest(
                "system_suffix is longer than 4000 characters".into(),
            ));
        }
    }
    if let Some(client) = &req.client {
        if client.is_empty()
            || client.len() > 32
            || !client
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(Error::InvalidRequest(
                "client must be 1-32 chars of [a-z0-9-]".into(),
            ));
        }
    }
    let (_, created) = state.control_plane.post("/sessions", &req).await?;
    tracing::info!(
        agent = %req.agent_slug,
        session = created["session_id"].as_str().unwrap_or(""),
        environment = created["environment"].as_str().unwrap_or(""),
        custom_tools = req.tools.len(),
        "session created"
    );
    Ok((StatusCode::CREATED, Json(created)))
}

// ---- list / get ---------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Only this agent's sessions.
    pub agent_slug: Option<String>,
    /// Page size; upstream's default applies when absent.
    pub limit: Option<u32>,
    /// Cursor from a previous response's `next_page`.
    pub page: Option<String>,
    /// `asc` or `desc` (default) by creation.
    pub order: Option<String>,
}

/// Anthropic's list envelope: `{"data":[…],"next_page":…,"prev_page":…}`.
#[derive(Serialize, utoipa::ToSchema)]
pub struct Page {
    pub data: Vec<Value>,
    #[schema(value_type = Option<String>)]
    pub next_page: Value,
    #[schema(value_type = Option<String>)]
    pub prev_page: Value,
}

#[utoipa::path(get, path = "/sessions", tag = "sessions", security(("api_key" = ["sessions:read"])),
    params(ListQuery),
    responses((status = 200, description = "Sessions, newest first, each with `console_url`", body = Page)))]
pub async fn list(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsRead)?;
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(s) = &q.agent_slug {
        query.push(("agent_slug", s.clone()));
    }
    if let Some(l) = q.limit {
        query.push(("limit", l.to_string()));
    }
    if let Some(p) = &q.page {
        query.push(("page", p.clone()));
    }
    if let Some(o) = &q.order {
        query.push(("order", o.clone()));
    }
    let q: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Ok(Json(state.control_plane.get("/sessions", &q).await?))
}

#[utoipa::path(get, path = "/sessions/{id}", tag = "sessions", security(("api_key" = ["sessions:read"])),
    params(("id" = String, Path, description = "sesn_…")),
    responses(
        (status = 200, description = "The Managed Agents session object plus `console_url`", body = Object),
        (status = 404, body = crate::openapi::ErrorBody),
    ))]
pub async fn get(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsRead)?;
    valid_id(&id)?;
    Ok(Json(
        state
            .control_plane
            .get(&format!("/sessions/{id}"), &[])
            .await?,
    ))
}

// ---- events -------------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct EventsQuery {
    pub page: Option<String>,
    pub limit: Option<u32>,
    /// Comma-separated event types, e.g. `agent.message,session.error`.
    pub types: Option<String>,
    /// `asc` (default) or `desc`. `desc` with `limit=1` is "the latest …".
    pub order: Option<String>,
}

#[utoipa::path(get, path = "/sessions/{id}/events", tag = "sessions", security(("api_key" = ["sessions:read"])),
    params(("id" = String, Path), EventsQuery),
    responses((status = 200, description = "Event history, oldest first unless `order=desc`", body = Page)))]
pub async fn events(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsRead)?;
    valid_id(&id)?;
    let mut query: Vec<(&str, String)> = Vec::new();
    if let Some(p) = &q.page {
        query.push(("page", p.clone()));
    }
    if let Some(l) = q.limit {
        query.push(("limit", l.to_string()));
    }
    if let Some(t) = &q.types {
        query.push(("types", t.clone()));
    }
    if let Some(o) = &q.order {
        query.push(("order", o.clone()));
    }
    let q: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Ok(Json(
        state
            .control_plane
            .get(&format!("/sessions/{id}/events"), &q)
            .await?,
    ))
}

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    /// A follow-up user message. Sending to an idle session resumes it.
    pub task: String,
}

#[utoipa::path(post, path = "/sessions/{id}/events", tag = "sessions", security(("api_key" = ["sessions:write"])),
    params(("id" = String, Path)),
    request_body = SendMessage,
    responses((status = 200, description = "The appended `user.message` event(s): `{\"data\":[…]}`", body = Object)))]
pub async fn send(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
    body: std::result::Result<Json<SendMessage>, JsonRejection>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsWrite)?;
    valid_id(&id)?;
    let req = body_or_400(body)?;
    if req.task.trim().is_empty() {
        return Err(Error::InvalidRequest("task must not be empty".into()));
    }
    let (_, out) = state
        .control_plane
        .post(&format!("/sessions/{id}/events"), &req)
        .await?;
    tracing::info!(session = %id, "message sent");
    Ok(Json(out))
}

#[utoipa::path(post, path = "/sessions/{id}/tool-results", tag = "sessions", security(("api_key" = ["sessions:write"])),
    params(("id" = String, Path)),
    request_body = ToolResults,
    responses((status = 200, description = "The appended `user.custom_tool_result` event(s): `{\"data\":[…]}`", body = Object)))]
pub async fn tool_results(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
    body: std::result::Result<Json<ToolResults>, JsonRejection>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsWrite)?;
    valid_id(&id)?;
    let req = body_or_400(body)?;
    validate_tool_results(&req)?;
    let (_, out) = state
        .control_plane
        .post(&format!("/sessions/{id}/tool-results"), &req)
        .await?;
    tracing::info!(session = %id, results = req.results.len(), "tool results sent");
    Ok(Json(out))
}

pub(super) fn validate_tool_results(req: &ToolResults) -> Result<()> {
    if req.results.is_empty() {
        return Err(Error::InvalidRequest("results must not be empty".into()));
    }
    for r in &req.results {
        valid_id(&r.custom_tool_use_id).map_err(|_| {
            Error::InvalidRequest("custom_tool_use_id has unexpected characters".into())
        })?;
    }
    Ok(())
}

#[utoipa::path(post, path = "/sessions/{id}/interrupt", tag = "sessions", security(("api_key" = ["sessions:write"])),
    params(("id" = String, Path)),
    responses((status = 200, description = "The appended `user.interrupt` event: `{\"data\":[…]}`", body = Object)))]
pub async fn interrupt(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsWrite)?;
    valid_id(&id)?;
    let (_, out) = state
        .control_plane
        .post(&format!("/sessions/{id}/interrupt"), &Value::Null)
        .await?;
    tracing::info!(session = %id, "session interrupted");
    Ok(Json(out))
}

// ---- stream -------------------------------------------------------------

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct StreamQuery {
    /// Comma-separated subset of `agent.message,agent.thinking` to also
    /// receive as incremental deltas.
    pub event_deltas: Option<String>,
}

#[utoipa::path(get, path = "/sessions/{id}/stream", tag = "sessions", security(("api_key" = ["sessions:read"])),
    params(("id" = String, Path), StreamQuery),
    responses((status = 200, description = "`text/event-stream`: a `: connected` comment, then `event: message` + `data: {event}` frames for events emitted after the stream opened. List `/events` afterwards and dedupe on `id` to fill history.", content_type = "text/event-stream")))]
pub async fn stream(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<StreamQuery>,
) -> Result<Response> {
    who.require(Scope::SessionsRead)?;
    valid_id(&id)?;
    let query: Vec<(&str, &str)> = q
        .event_deltas
        .as_deref()
        .map(|d| vec![("event_deltas", d)])
        .unwrap_or_default();
    let upstream = state
        .control_plane
        .open::<Value>(Method::GET, &format!("/sessions/{id}/stream"), &query, None)
        .await?;
    tracing::info!(session = %id, "event stream opened");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| Error::Config(format!("stream response: {e}")))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create, list))
        .routes(routes!(get))
        .routes(routes!(events, send))
        .routes(routes!(tool_results))
        .routes(routes!(interrupt))
        .routes(routes!(stream))
}
