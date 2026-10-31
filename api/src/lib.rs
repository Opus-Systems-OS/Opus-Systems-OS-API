//! Opus Systems OS API — library half. `main.rs` is the CLI/bin; everything
//! testable lives here so integration tests can build the real router.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod openapi;
pub mod request_id;
pub mod sse;
pub mod upstream;
pub mod v1;

use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::Router;
use std::time::Duration;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::timeout::TimeoutLayer;

/// Longest a handler may take to produce its response head — above the
/// longest upstream budget (a 120 s file upload). It ends at the head, so a
/// stream or WebSocket runs on after it for as long as it stays open.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
/// Requests being handled at once, across every route; more wait their turn.
pub const MAX_IN_FLIGHT: usize = 512;

/// The complete application: `/v1` plus request ids, tracing, a timeout and
/// a concurrency cap, and — when `allowed_origins` is non-empty — CORS for
/// those exact origins.
pub fn app(state: v1::AppState, allowed_origins: &[String]) -> Router {
    let mut router = v1::router(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(axum::middleware::from_fn(request_id::layer))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // Global: `Router::layer` wraps each route, so a per-route limit
        // would not cap the whole.
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT));
    if !allowed_origins.is_empty() {
        let origins: Vec<HeaderValue> = allowed_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect();
        router = router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
                .allow_headers([
                    header::AUTHORIZATION,
                    header::CONTENT_TYPE,
                    http::HeaderName::from_static(request_id::HEADER),
                ])
                .expose_headers([
                    header::RETRY_AFTER,
                    header::CONTENT_DISPOSITION,
                    http::HeaderName::from_static(request_id::HEADER),
                ])
                .max_age(std::time::Duration::from_secs(600)),
        );
    }
    router
}
