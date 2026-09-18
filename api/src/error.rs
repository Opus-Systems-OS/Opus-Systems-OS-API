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
    /// `kind` is upstream's type, which becomes ours when it is one a client
    /// can act on (`rig_offline`, `invalid_request`, `not_found`).
    #[error("upstream {status}: {kind}: {message}")]
    Upstream {
        status: u16,
        kind: String,
        message: String,
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
            Error::Upstream { kind, .. } => match kind.as_str() {
                "rig_offline" => "rig_offline",
                "invalid_request" | "unknown_agent" | "unknown_environment" => "invalid_request",
                "environment_not_provisioned" => "invalid_request",
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
            // failure to get an answer.
            Error::Upstream { status, .. } => match *status {
                400 | 404 | 409 | 503 => {
                    StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
                }
                401 => StatusCode::BAD_GATEWAY, // our token is wrong, not the caller's
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
        if let Error::RateLimited { retry_after_secs } = &self {
            if let Ok(v) = retry_after_secs.to_string().parse() {
                res.headers_mut().insert(http::header::RETRY_AFTER, v);
            }
        }
        res.extensions_mut().insert(self.envelope());
        res
    }
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
