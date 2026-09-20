//! The OpenAPI document. Routes and schemas are collected by `utoipa_axum`
//! from the routers; this holds what is not attached to a route — the
//! title, the security scheme, the shared error body.

use serde::Serialize;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// `{"error":{"type","message","request_id"}}` — every non-2xx.
#[derive(Serialize, utoipa::ToSchema)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct ErrorDetail {
    /// One of: unauthorized, forbidden, not_found, conflict, invalid_request,
    /// method_not_allowed, rate_limited, upstream, rig_offline, internal.
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
    /// Echoed from / set as the `x-request-id` header. Quote it when reporting.
    pub request_id: String,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Opus Systems OS API",
        description = "The front door to Opus Systems OS: the agent fleet, its sessions and usage, \
                       and local inference on the rig. Bearer keys of the form `osk_<id>_<secret>`; \
                       every error is `{\"error\":{\"type\",\"message\",\"request_id\"}}`.",
        license(name = "UNLICENSED"),
    ),
    servers((url = "https://api.opustower.dev/v1"), (url = "http://localhost:8100/v1")),
    modifiers(&ApiKeyScheme),
    components(schemas(ErrorBody, ErrorDetail)),
    tags(
        (name = "health", description = "Liveness"),
        (name = "auth", description = "The presented key"),
        (name = "keys", description = "Key administration (keys:admin)"),
        (name = "fleet", description = "The fleet as configured, and the rig (fleet:read)"),
        (name = "sessions", description = "Managed Agents sessions (sessions:read / sessions:write)"),
        (name = "usage", description = "Spend rollups and the CSV audit trail (usage:read)"),
        (name = "inference", description = "Local models on the rig (inference)"),
        (name = "voice", description = "Jarvis's voice — text to speech (voice)"),
        (name = "ops", description = "The stack's services at a glance — GitHub, UptimeRobot, the droplet, Docker, Tailscale, Cloudflare (ops:read)"),
    )
)]
pub struct Doc;

struct ApiKeyScheme;

impl Modify for ApiKeyScheme {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "api_key",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("osk_<id>_<secret>")
                    .build(),
            ),
        );
    }
}
