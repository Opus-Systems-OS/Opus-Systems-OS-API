//! `GET /v1/sessions/{id}/ws` — one WebSocket per session, for clients
//! without SSE (a Unity headset has `ClientWebSocket` and nothing else).
//!
//! Every frame is a JSON object with a `type`:
//!
//! server → client
//! - `{"type":"hello","session_id":…,"request_id":…}` — first frame.
//! - `{"type":"event","event":{…}}` — a session event, the same object the
//!   SSE stream and `/events` carry. History first (oldest first, unless
//!   `?history=false`), then live, deduplicated on `event.id`.
//! - `{"type":"sent","data":[…]}` — the events appended by a client frame.
//! - `{"type":"error","error":{"type":…,"message":…}}` — a client frame was
//!   rejected; the socket stays open.
//! - `{"type":"pong"}` — answer to `ping`.
//! - `{"type":"closed","reason":…}` — last frame before the server closes.
//!
//! client → server
//! - `{"type":"message","task":"…"}` (needs `sessions:write`)
//! - `{"type":"interrupt"}` (needs `sessions:write`)
//! - `{"type":"ping"}`
//!
//! Connecting needs `sessions:read`. The bearer goes in the `Authorization`
//! header of the upgrade request, as on every other route.
//!
//! Ordering is the documented reconnect pattern: open the live stream
//! *first*, then list history, then forward live events that history did
//! not already contain — so nothing emitted between the two calls is lost.

use super::AppState;
use crate::auth::keys::Scope;
use crate::auth::middleware::Principal;
use crate::error::{Error, Result};
use crate::request_id::RequestId;
use crate::sse::SseParser;
use crate::upstream::control_plane::ControlPlane;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::sync::mpsc;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct WsQuery {
    /// Send the session's history before live events. Default true.
    pub history: Option<bool>,
}

#[utoipa::path(get, path = "/sessions/{id}/ws", tag = "sessions", security(("api_key" = ["sessions:read"])),
    params(("id" = String, Path), WsQuery),
    responses(
        (status = 101, description = "WebSocket. JSON frames with a `type`: server sends `hello`, then `event` frames (history, then live), `sent`/`error`/`pong` in answer to client frames, `closed` last. Client sends `message` {task} and `interrupt` (sessions:write) and `ping`."),
        (status = 401, body = crate::openapi::ErrorBody),
        (status = 403, body = crate::openapi::ErrorBody),
    ))]
pub async fn ws(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Extension(RequestId(request_id)): Extension<RequestId>,
    Path(id): Path<String>,
    Query(q): Query<WsQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response> {
    who.require(Scope::SessionsRead)?;
    super::sessions::valid_id(&id)?;
    // Open the live stream before upgrading, so an unknown session is a
    // clean 404 envelope rather than a socket that closes at once.
    let upstream = state
        .control_plane
        .open::<Value>(Method::GET, &format!("/sessions/{id}/stream"), &[], None)
        .await?;
    let history = q.history.unwrap_or(true);
    tracing::info!(session = %id, history, "websocket opened");
    let cp = state.control_plane.clone();
    Ok(upgrade.on_upgrade(move |socket| run(socket, cp, who, id, request_id, upstream, history)))
}

fn frame(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

fn error_frame(e: &Error) -> Message {
    let env = e.envelope();
    frame(json!({"type": "error", "error": {"type": env.kind, "message": env.message}}))
}

async fn run(
    socket: WebSocket,
    cp: ControlPlane,
    who: Principal,
    session_id: String,
    request_id: String,
    upstream: reqwest::Response,
    history: bool,
) {
    let (mut tx, mut rx) = socket.split();

    // Live events, parsed off the SSE bytes on their own task so they buffer
    // while history is being listed.
    let (live_tx, mut live_rx) = mpsc::channel::<Value>(256);
    let reader = tokio::spawn(async move {
        let mut bytes = upstream.bytes_stream();
        let mut parser = SseParser::default();
        while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            parser.push(&chunk);
            for f in parser.frames() {
                if let Ok(v) = serde_json::from_str::<Value>(&f.data) {
                    if live_tx.send(v).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let hello = json!({"type": "hello", "session_id": session_id, "request_id": request_id});
    if tx.send(frame(hello)).await.is_err() {
        reader.abort();
        return;
    }

    let mut seen: HashSet<String> = HashSet::new();
    if history {
        match list_history(&cp, &session_id).await {
            Ok(events) => {
                for ev in events {
                    if let Some(id) = ev["id"].as_str() {
                        seen.insert(id.to_owned());
                    }
                    if tx
                        .send(frame(json!({"type": "event", "event": ev})))
                        .await
                        .is_err()
                    {
                        reader.abort();
                        return;
                    }
                }
            }
            Err(e) => {
                let _ = tx.send(error_frame(&e)).await;
            }
        }
    }

    let reason = loop {
        tokio::select! {
            live = live_rx.recv() => match live {
                Some(ev) => {
                    if let Some(id) = ev["id"].as_str() {
                        if !seen.insert(id.to_owned()) {
                            continue;
                        }
                    }
                    if tx.send(frame(json!({"type": "event", "event": ev}))).await.is_err() {
                        break "client_closed";
                    }
                }
                None => break "upstream_closed",
            },
            incoming = rx.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    let reply = handle_client_frame(&cp, &who, &session_id, &text).await;
                    if tx.send(reply).await.is_err() {
                        break "client_closed";
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break "client_closed",
                Some(Ok(_)) => {} // binary, ping/pong handled by axum
            },
        }
    };
    reader.abort();
    if reason != "client_closed" {
        let _ = tx
            .send(frame(json!({"type": "closed", "reason": reason})))
            .await;
        let _ = tx.send(Message::Close(None)).await;
    }
    tracing::info!(session = %session_id, reason, "websocket closed");
}

/// Every page of `/events`, oldest first.
async fn list_history(cp: &ControlPlane, session_id: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let mut query: Vec<(&str, &str)> = vec![("limit", "1000")];
        if let Some(p) = page.as_deref() {
            query.push(("page", p));
        }
        let envelope = cp
            .get(&format!("/sessions/{session_id}/events"), &query)
            .await?;
        if let Some(items) = envelope["data"].as_array() {
            out.extend(items.iter().cloned());
        }
        match envelope["next_page"].as_str() {
            Some(next) if !next.is_empty() => page = Some(next.to_owned()),
            _ => return Ok(out),
        }
    }
}

async fn handle_client_frame(
    cp: &ControlPlane,
    who: &Principal,
    session_id: &str,
    text: &str,
) -> Message {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return error_frame(&Error::InvalidRequest(format!("frame is not JSON: {e}"))),
    };
    let kind = parsed["type"].as_str().unwrap_or("");
    let result: Result<Message> = match kind {
        "ping" => Ok(frame(json!({"type": "pong"}))),
        "message" => {
            async {
                who.require(Scope::SessionsWrite)?;
                let task = parsed["task"].as_str().map(str::trim).unwrap_or("");
                if task.is_empty() {
                    return Err(Error::InvalidRequest(
                        "`task` must be a non-empty string".into(),
                    ));
                }
                let (_, out) = cp
                    .post(
                        &format!("/sessions/{session_id}/events"),
                        &json!({"task": task}),
                    )
                    .await?;
                Ok(frame(json!({"type": "sent", "data": out["data"]})))
            }
            .await
        }
        "interrupt" => {
            async {
                who.require(Scope::SessionsWrite)?;
                let (_, out) = cp
                    .post(&format!("/sessions/{session_id}/interrupt"), &Value::Null)
                    .await?;
                Ok(frame(json!({"type": "sent", "data": out["data"]})))
            }
            .await
        }
        other => Err(Error::InvalidRequest(format!(
            "unknown frame type `{other}`; expected message, interrupt or ping"
        ))),
    };
    match result {
        Ok(m) => m,
        Err(e) => error_frame(&e),
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(ws))
}
