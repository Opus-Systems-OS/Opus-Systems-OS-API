//! Opus Systems OS API — library half. `main.rs` is the CLI/bin; everything
//! testable lives here so integration tests can build the real router.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod openapi;
pub mod request_id;
pub mod v1;

use axum::Router;

/// The complete application: `/v1` plus the request-id and trace layers.
pub fn app(state: v1::AppState) -> Router {
    v1::router(state)
        .layer(axum::middleware::from_fn(request_id::layer))
        .layer(tower_http::trace::TraceLayer::new_for_http())
}
