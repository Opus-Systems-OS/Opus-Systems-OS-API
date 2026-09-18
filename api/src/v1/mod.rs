//! `/v1` — the contract. Additive changes only; anything breaking is `/v2`.
//!
//! Layout: a public router (`/v1/health`, the spec, the docs) and a protected
//! one behind `authenticate`, in which each area is its own sub-router with
//! its scope applied as a `route_layer`. Every route is registered through
//! `utoipa_axum` so the spec and the router cannot drift apart.

pub mod health;
pub mod keys;
pub mod me;

use crate::auth::keys::Scope;
use crate::auth::middleware::{authenticate, require, AuthState};
use crate::db::Db;
use crate::error::Error;
use axum::middleware::from_fn_with_state;
use axum::Router;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_redoc::{Redoc, Servable};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
}

/// A sub-router whose every route needs `scope`.
fn scoped(scope: Scope, router: OpenApiRouter<AppState>) -> OpenApiRouter<AppState> {
    router.route_layer(from_fn_with_state(scope, require))
}

pub fn router(state: AppState) -> Router {
    let auth = Arc::new(AuthState {
        db: state.db.clone(),
    });

    let protected = OpenApiRouter::new()
        .merge(me::router())
        .merge(scoped(Scope::KeysAdmin, keys::router()))
        .route_layer(from_fn_with_state(auth, authenticate));

    let (router, api) = OpenApiRouter::with_openapi(crate::openapi::Doc::openapi())
        .merge(health::router())
        .merge(protected)
        .split_for_parts();

    let spec = api.clone();
    Router::new()
        .nest(
            "/v1",
            router
                .route(
                    "/openapi.json",
                    axum::routing::get(move || {
                        let spec = spec.clone();
                        async move { axum::Json(spec) }
                    }),
                )
                .merge(Redoc::with_url("/docs", api)),
        )
        .fallback(|| async { Error::NotFound })
        .with_state(state)
}
