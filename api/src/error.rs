//! One error type, one wire shape. Every failure a client can see leaves as
//!
//! ```json
//! {"error":{"type":"unauthorized","message":"…","request_id":"req_…"}}
//! ```
//!
//! `Error::into_response` sets the status and attaches the envelope *without*
//! the request id as a response extension; `request_id::layer` owns the final
//! body, because only it knows the id. axum's own rejections (bad JSON, unknown
//! route, wrong method) arrive as plain text and are folded into the same
//! envelope there too — a client never has to parse two shapes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // Startup only — never reaches a response.
    #[error("config: {0}")]
    Config(String),

    // Caller errors.
    #[error("unauthorized")]
    Unauthorized,
    #[error("missing scope `{0}`")]
    Forbidden(&'static str),
    #[error("not found")]
    NotFound,
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("rate limited")]
    RateLimited { retry_after_secs: u32 },

    // Our side / upstream.
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    /// A non-2xx from the control plane, already reduced to its own
    /// `{error:{type,message}}` body. `status` is the upstream status;
    /// `kind` is upstream's type, mapped to ours in `kind()`.
    #[error("upstream {status}: {kind}: {message}")]
    Upstream {
        status: u16,
        kind: String,
        message: String,
        retry_after: Option<u32>,
    },
    #[error("upstream transport: {0}")]
    UpstreamTransport(#[from] reqwest::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The envelope body minus the request id, carried as a response extension
/// from `Error::into_response` to `request_id::layer`.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

impl Error {
    /// Stable machine-readable `type` for the envelope.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::Config(_) => "internal",
            Error::Unauthorized => "unauthorized",
            Error::Forbidden(_) => "forbidden",
            Error::NotFound => "not_found",
            Error::InvalidRequest(_) => "invalid_request",
            Error::RateLimited { .. } => "rate_limited",
            Error::Db(_) => "internal",
            // The control plane's kinds, in our vocabulary. Anything it
            // reports about *itself* (database, config, Anthropic transport)
            // is an `upstream` failure from the client's point of view.
            Error::Upstream {
                kind,
                status,
                message,
                ..
            } => match kind.as_str() {
                "rig_offline" => "rig_offline",
                "invalid_request" => "invalid_request",
                "unknown_agent" | "unknown_environment" => "not_found",
                "environment_not_provisioned" => "conflict",
                // Ollama's own 4xx (unknown model, bad body) passes through
                // the control plane as `inference`; it is the caller's.
                "inference" if (400..500).contains(status) => "invalid_request",
                // Anthropic's own errors arrive as `upstream` with the
                // message `<anthropic type>: <text>` (the control plane
                // renders any non-404 of them as 502). The caller-side ones
                // are the caller's: a malformed session id is a 400, not a
                // gateway failure.
                "upstream" => match anthropic_kind(message) {
                    Some("invalid_request_error") => "invalid_request",
                    Some("not_found_error") => "not_found",
                    _ if *status == 404 => "not_found",
                    _ => "upstream",
                },
                _ => "upstream",
            },
            Error::UpstreamTransport(_) => "upstream",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Error::Config(_) | Error::Db(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Error::Unauthorized => StatusCode::UNAUTHORIZED,
            Error::Forbidden(_) => StatusCode::FORBIDDEN,
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Error::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            // The control plane's status is meaningful to the caller for the
            // client-side kinds (400/404/409/503); the rest is our gateway's
            // failure to get an answer — including 401, which means *our*
            // token is wrong, not the caller's.
            Error::Upstream { status, .. } => match (self.kind(), *status) {
                ("invalid_request", _) => StatusCode::BAD_REQUEST,
                ("not_found", _) => StatusCode::NOT_FOUND,
                ("conflict", _) => StatusCode::CONFLICT,
                ("rig_offline", _) => StatusCode::SERVICE_UNAVAILABLE,
                (_, 503) => StatusCode::SERVICE_UNAVAILABLE,
                _ => StatusCode::BAD_GATEWAY,
            },
            Error::UpstreamTransport(_) => StatusCode::BAD_GATEWAY,
        }
    }

    /// Message safe to return. Internal failures are not echoed.
    fn public_message(&self) -> String {
        match self {
            Error::Config(_) | Error::Db(_) => "internal error".to_owned(),
            Error::UpstreamTransport(_) => "could not reach the control plane".to_owned(),
            Error::Upstream { status: 401, .. } => {
                "the gateway's control-plane credential was rejected".to_owned()
            }
            Error::Upstream { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }

    pub fn envelope(&self) -> Envelope {
        Envelope {
            kind: self.kind().to_owned(),
            message: self.public_message(),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, kind = self.kind(), "request failed");
        } else {
            tracing::warn!(error = %self, kind = self.kind(), "request rejected");
        }
        let mut res = status.into_response();
        let retry_after = match &self {
            Error::RateLimited { retry_after_secs } => Some(*retry_after_secs),
            Error::Upstream {
                retry_after: Some(s),
                ..
            } => Some(*s),
            // A rig that is off answers in ~5 s (the connect timeout); tell
            // the client not to hammer it even if upstream forgot the header.
            Error::Upstream { kind, .. } if kind == "rig_offline" => Some(5),
            _ => None,
        };
        if let Some(secs) = retry_after {
            if let Ok(v) = secs.to_string().parse() {
                res.headers_mut().insert(http::header::RETRY_AFTER, v);
            }
        }
        res.extensions_mut().insert(self.envelope());
        res
    }
}

/// The Anthropic error type at the front of a control-plane `upstream`
/// message (`"invalid_request_error: Invalid session ID: …"`), if any.
fn anthropic_kind(message: &str) -> Option<&str> {
    let (head, _) = message.split_once(": ")?;
    (head.ends_with("_error") && head.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        .then_some(head)
}

/// Envelope for a response axum produced on its own (extractor rejections,
/// no matching route or method). The `type` follows the status; the text
/// axum wrote is the message.
pub fn envelope_for_status(status: StatusCode, text: &str) -> Envelope {
    let kind = match status {
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::UNAUTHORIZED => "unauthorized",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::CONFLICT => "conflict",
        s if s.is_client_error() => "invalid_request",
        _ => "internal",
    };
    let message = match status {
        StatusCode::NOT_FOUND if text.is_empty() => "no such route".to_owned(),
        StatusCode::METHOD_NOT_ALLOWED if text.is_empty() => "method not allowed".to_owned(),
        _ if text.is_empty() => status
            .canonical_reason()
            .unwrap_or("error")
            .to_ascii_lowercase(),
        _ => text.to_owned(),
    };
    Envelope {
        kind: kind.to_owned(),
        message,
    }
}
