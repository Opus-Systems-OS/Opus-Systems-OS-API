//! `/v1/files` — attach files to a session message, and fetch what an agent
//! hands back. Uploading needs `sessions:write`, reading `sessions:read`.
//!
//! The upload is passed to the control plane as it came (multipart, one
//! part named `file`), which sends it to Anthropic's Files API and records
//! the id; only ids recorded there are accepted as `attachments`. Nothing is
//! stored here.

use super::sessions::valid_id;
use super::AppState;
use crate::auth::keys::Scope;
use crate::auth::middleware::Principal;
use crate::error::{Error, Result};
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use reqwest::Method;
use serde::Serialize;
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// The control plane's per-file limit, plus multipart framing.
const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024 + 64 * 1024;

pub fn valid_file_id(id: &str) -> bool {
    id.len() > 5 && id.starts_with("file_") && valid_id(id).is_ok()
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Uploaded {
    /// Pass this in a session's `attachments`.
    pub file_id: String,
    /// Cleaned to `[A-Za-z0-9._-]`; the file's name in the sandbox.
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
}

#[utoipa::path(post, path = "/files", tag = "files", security(("api_key" = ["sessions:write"])),
    request_body(content = Vec<u8>, description = "`multipart/form-data` with one part named `file`, ≤ 32 MB.", content_type = "multipart/form-data"),
    responses(
        (status = 201, body = Uploaded),
        (status = 400, description = "not multipart, no `file` part, empty, or over 32 MB", body = crate::openapi::ErrorBody),
        (status = 413, description = "over 32 MB", body = crate::openapi::ErrorBody),
    ))]
pub async fn upload(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>)> {
    who.require(Scope::SessionsWrite)?;
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ctype
        .to_ascii_lowercase()
        .starts_with("multipart/form-data;")
    {
        return Err(Error::InvalidRequest(
            "content-type must be multipart/form-data".into(),
        ));
    }
    let bytes = body.len();
    let (status, out) = state
        .control_plane
        .post_bytes("/files", ctype, body)
        .await?;
    tracing::info!(
        file = out["file_id"].as_str().unwrap_or(""),
        bytes,
        "file uploaded"
    );
    Ok((status, Json(out)))
}

#[utoipa::path(get, path = "/sessions/{id}/files", tag = "files", security(("api_key" = ["sessions:read"])),
    params(("id" = String, Path)),
    responses((status = 200, description = "Anthropic's file list for the session (`data`: `id`, `filename`, `mime_type`, `size_bytes`, `created_at`, `downloadable`). Files the agent wrote to `/mnt/session/outputs/` are `downloadable`; the session's copies of its uploads are not.", body = Object)))]
pub async fn session_files(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    who.require(Scope::SessionsRead)?;
    valid_id(&id)?;
    Ok(Json(
        state
            .control_plane
            .get(&format!("/sessions/{id}/files"), &[])
            .await?,
    ))
}

#[utoipa::path(get, path = "/files/{id}/content", tag = "files", security(("api_key" = ["sessions:read"])),
    params(("id" = String, Path, description = "A `downloadable` file id from `/sessions/{id}/files`")),
    responses(
        (status = 200, description = "The file, with `Content-Disposition: attachment; filename=…`", content_type = "application/octet-stream"),
        (status = 400, description = "an upload (only agent outputs download)", body = crate::openapi::ErrorBody),
    ))]
pub async fn content(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Response> {
    who.require(Scope::SessionsRead)?;
    if !valid_file_id(&id) {
        return Err(Error::InvalidRequest(
            "file id has unexpected characters".into(),
        ));
    }
    let upstream = state
        .control_plane
        .open::<Value>(Method::GET, &format!("/files/{id}/content"), &[], None)
        .await?;
    let mut res = Response::builder().status(StatusCode::OK);
    for name in [header::CONTENT_TYPE, header::CONTENT_DISPOSITION] {
        if let Some(v) = upstream.headers().get(&name) {
            res = res.header(name, v);
        }
    }
    res.body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| Error::Config(format!("file response: {e}")))
}

pub fn router() -> OpenApiRouter<AppState> {
    let upload = OpenApiRouter::new()
        .routes(routes!(upload))
        .route_layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES));
    OpenApiRouter::new()
        .routes(routes!(session_files))
        .routes(routes!(content))
        .merge(upload)
}
