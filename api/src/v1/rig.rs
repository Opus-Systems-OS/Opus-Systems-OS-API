//! `GET /v1/rig` — is the GPU rig there? Composed from the control plane's
//! `/inference/models` status so every client gets the same three-way
//! answer without parsing an error: not configured / offline / online with
//! model names. This is what the desktop apps' Fleet tab shows.

use super::AppState;
use crate::error::Result;
use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Serialize, utoipa::ToSchema)]
pub struct Rig {
    /// False when this deployment has no local inference at all.
    pub configured: bool,
    pub online: bool,
    /// Ollama model tags when online, e.g. `["qwen3:8b", "nomic-embed-text:latest"]`.
    pub models: Vec<String>,
    /// Why it is offline, when it is.
    pub reason: Option<String>,
}

#[utoipa::path(get, path = "/rig", tag = "fleet", security(("api_key" = ["fleet:read"])),
    responses((status = 200, body = Rig)))]
pub async fn rig(State(state): State<AppState>) -> Result<Json<Rig>> {
    let res = state.control_plane.get_raw("/inference/models").await?;
    let status = res.status().as_u16();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    let rig = match status {
        404 => Rig {
            configured: false,
            online: false,
            models: vec![],
            reason: None,
        },
        200 => Rig {
            configured: true,
            online: true,
            models: body["models"]
                .as_array()
                .map(|ms| {
                    ms.iter()
                        .filter_map(|m| m["name"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            reason: None,
        },
        _ => Rig {
            configured: true,
            online: false,
            models: vec![],
            reason: Some(
                body["error"]["message"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("control plane answered {status}")),
            ),
        },
    };
    Ok(Json(rig))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(rig))
}
