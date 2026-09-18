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

use axum::http::{header, HeaderValue, Method};
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};

/// The complete application: `/v1` plus request ids, tracing and — when
/// `allowed_origins` is non-empty — CORS for those exact origins.
pub fn app(state: v1::AppState, allowed_origins: &[String]) -> Router {
    let mut router = v1::router(state)
        .layer(axum::middleware::from_fn(request_id::layer))
        .layer(tower_http::trace::TraceLayer::new_for_http());
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
