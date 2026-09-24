//! `/v1/voice` — Jarvis's voice and ears. Registered only when
//! `FISH_AUDIO_API_KEY` is set. A client sends text and gets audio bytes
//! (which voice speaks is server configuration, so every client sounds the
//! same), or sends a recording and gets the text — for browsers with no
//! speech service of their own. What was said is never logged.

use super::AppState;
use crate::error::{Error, Result};
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

const TEXT_MAX_CHARS: usize = 2_000;

#[derive(Deserialize, Serialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Speak {
    /// What to say. 1–2000 characters; plain text, no markdown.
    pub text: String,
    /// `mp3` (default), `wav`, `pcm`, `opus`.
    #[serde(default)]
    pub format: Option<String>,
    /// `low` (default here — replies are spoken as they arrive), `normal`, `balanced`.
    #[serde(default)]
    pub latency: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct VoiceInfo {
    pub configured: bool,
    /// The Fish Audio reference id Jarvis speaks with.
    pub voice_id: String,
    pub model: Option<String>,
}

fn one_of<'a>(
    value: Option<&'a str>,
    allowed: &[&'a str],
    default: &'a str,
    name: &str,
) -> Result<&'a str> {
    match value {
        None => Ok(default),
        Some(v) if allowed.contains(&v) => Ok(v),
        Some(v) => Err(Error::InvalidRequest(format!(
            "{name} `{v}` is not one of {}",
            allowed.join(", ")
        ))),
    }
}

#[utoipa::path(post, path = "/voice/speak", tag = "voice", security(("api_key" = ["voice"])),
    request_body = Speak,
    responses(
        (status = 200, description = "Audio bytes, streamed; `content-type` matches `format` (`audio/mpeg` for mp3)", content_type = "audio/mpeg"),
        (status = 400, body = crate::openapi::ErrorBody),
        (status = 502, description = "the voice provider rejected the request (key, credits)", body = crate::openapi::ErrorBody),
        (status = 503, description = "the voice provider is overloaded; `Retry-After`", body = crate::openapi::ErrorBody),
    ))]
pub async fn speak(
    State(state): State<AppState>,
    body: std::result::Result<Json<Speak>, JsonRejection>,
) -> Result<Response> {
    let voice = state.voice.as_ref().as_ref().ok_or(Error::NotFound)?;
    let Json(req) = body.map_err(|e| Error::InvalidRequest(e.body_text()))?;
    let text = req.text.trim();
    let n = text.chars().count();
    if n == 0 || n > TEXT_MAX_CHARS {
        return Err(Error::InvalidRequest(format!(
            "text must be 1–{TEXT_MAX_CHARS} characters"
        )));
    }
    let format = one_of(
        req.format.as_deref(),
        &["mp3", "wav", "pcm", "opus"],
        "mp3",
        "format",
    )?;
    let latency = one_of(
        req.latency.as_deref(),
        &["low", "normal", "balanced"],
        "low",
        "latency",
    )?;
    let upstream = voice.speak(text, format, latency).await?;
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| header::HeaderValue::from_static("application/octet-stream"));
    tracing::info!(chars = n, format, latency, "voice speak");
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|e| Error::Config(format!("voice response: {e}")))
}

#[utoipa::path(get, path = "/voice", tag = "voice", security(("api_key" = ["voice"])),
    responses((status = 200, body = VoiceInfo)))]
pub async fn info(State(state): State<AppState>) -> Result<Json<VoiceInfo>> {
    let voice = state.voice.as_ref().as_ref().ok_or(Error::NotFound)?;
    Ok(Json(VoiceInfo {
        configured: true,
        voice_id: voice.voice_id().to_owned(),
        model: voice.model().map(str::to_owned),
    }))
}

#[utoipa::path(get, path = "/voice/credit", tag = "voice", security(("api_key" = ["voice"])),
    responses(
        (status = 200, description = "Fish Audio API credit; read at most every 5 minutes", body = crate::upstream::fish_audio::VoiceCredit),
        (status = 502, description = "the voice provider rejected the request (key)", body = crate::openapi::ErrorBody),
        (status = 503, description = "the voice provider is overloaded; `Retry-After`", body = crate::openapi::ErrorBody),
    ))]
pub async fn credit(
    State(state): State<AppState>,
) -> Result<Json<crate::upstream::fish_audio::VoiceCredit>> {
    let voice = state.voice.as_ref().as_ref().ok_or(Error::NotFound)?;
    Ok(Json(voice.credit().await?))
}

/// A recording's upper bound: 1 MiB is ~30 s of 16 kHz mono WAV.
const AUDIO_MAX_BYTES: usize = 1024 * 1024;
/// Containers Fish decodes (WebM is not one — clients send WAV).
const AUDIO_TYPES: &[&str] = &[
    "audio/wav",
    "audio/x-wav",
    "audio/wave",
    "audio/mpeg",
    "audio/mp4",
];

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct TranscribeQuery {
    /// Two-letter language hint; `en` when absent.
    pub language: Option<String>,
}

#[utoipa::path(post, path = "/voice/transcribe", tag = "voice", security(("api_key" = ["voice"])),
    params(TranscribeQuery),
    request_body(content = Vec<u8>, description = "One utterance: WAV (16 kHz mono recommended), MP3 or M4A; ≤ 1 MiB. `content-type` names the format.", content_type = "audio/wav"),
    responses(
        (status = 200, body = crate::upstream::fish_audio::Transcription),
        (status = 400, description = "empty, unsupported or undecodable audio", body = crate::openapi::ErrorBody),
        (status = 413, description = "over 1 MiB", body = crate::openapi::ErrorBody),
        (status = 502, description = "the voice provider rejected the request (key, credits)", body = crate::openapi::ErrorBody),
        (status = 503, description = "the voice provider is overloaded; `Retry-After`", body = crate::openapi::ErrorBody),
    ))]
pub async fn transcribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<TranscribeQuery>,
    body: Bytes,
) -> Result<Json<crate::upstream::fish_audio::Transcription>> {
    let voice = state.voice.as_ref().as_ref().ok_or(Error::NotFound)?;
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();
    if !AUDIO_TYPES.contains(&mime.as_str()) {
        return Err(Error::InvalidRequest(format!(
            "content-type must be one of {}",
            AUDIO_TYPES.join(", ")
        )));
    }
    if body.is_empty() {
        return Err(Error::InvalidRequest("audio must not be empty".into()));
    }
    let language = q.language.unwrap_or_else(|| "en".to_owned());
    if language.len() != 2 || !language.bytes().all(|b| b.is_ascii_lowercase()) {
        return Err(Error::InvalidRequest(
            "language must be a two-letter code like `en`".into(),
        ));
    }
    let bytes = body.len();
    let out = voice.transcribe(body, &mime, &language).await?;
    // Size and length only: what was said stays out of the logs.
    tracing::info!(bytes, mime, duration = ?out.duration, chars = out.text.chars().count(), "voice transcribe");
    Ok(Json(out))
}

pub fn router() -> OpenApiRouter<AppState> {
    let text = OpenApiRouter::new()
        .routes(routes!(speak))
        .routes(routes!(info))
        .routes(routes!(credit))
        .route_layer(DefaultBodyLimit::max(64 * 1024));
    let audio = OpenApiRouter::new()
        .routes(routes!(transcribe))
        .route_layer(DefaultBodyLimit::max(AUDIO_MAX_BYTES));
    text.merge(audio)
}
