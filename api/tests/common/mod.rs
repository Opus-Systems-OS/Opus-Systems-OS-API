//! Test harness: the real API router over an in-memory DB, pointed at a
//! stub control plane (an axum server on an ephemeral port) that answers the
//! routes stage 2 proxies, with canned bodies modelled on the live one.

#![allow(dead_code)]

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use opus_api::auth::keys::Scope;
use opus_api::auth::rate_limit::RateLimiter;
use opus_api::config::VoiceConfig;
use opus_api::db::Db;
use opus_api::upstream::control_plane::ControlPlane;
use opus_api::upstream::fish_audio::FishAudio;
use opus_api::v1::keys::create_key;
use opus_api::v1::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub const CP_TOKEN: &str = "cp-secret";
pub const FISH_KEY: &str = "sk-fish-test";

/// What the stub saw, for assertions about what reached upstream.
#[derive(Default)]
pub struct Seen {
    pub requests: Vec<(String, String, Option<Value>)>, // method, path+query, body
    pub authorized: bool,
}

#[derive(Clone)]
struct StubState {
    seen: Arc<Mutex<Seen>>,
    /// `Some(status, body)` makes `/inference/models` answer that instead of 200.
    rig: Arc<Mutex<Option<(u16, Value)>>>,
    /// Live events for the stub's SSE stream; tests push into it.
    live: tokio::sync::broadcast::Sender<Value>,
}

#[derive(Default)]
pub struct Options {
    /// 0 = disabled (the default for tests).
    pub rate_limit_per_minute: u32,
    pub allowed_origins: Vec<String>,
    /// Register `/v1/voice/*` against a stub Fish Audio.
    pub voice: bool,
}

pub struct Harness {
    pub app: axum::Router,
    pub db: Db,
    pub seen: Arc<Mutex<Seen>>,
    /// The app served on a real port, for WebSocket tests.
    pub addr: std::net::SocketAddr,
    rig: Arc<Mutex<Option<(u16, Value)>>>,
    live: tokio::sync::broadcast::Sender<Value>,
}

impl Harness {
    pub fn key(&self, name: &str, scopes: &[Scope]) -> String {
        create_key(&self.db, name, scopes).unwrap().key
    }

    pub fn all_scopes_key(&self) -> String {
        self.key(
            "all",
            &[
                Scope::FleetRead,
                Scope::SessionsRead,
                Scope::SessionsWrite,
                Scope::UsageRead,
                Scope::Inference,
            ],
        )
    }

    pub fn rig_answers(&self, status: u16, body: Value) {
        *self.rig.lock().unwrap() = Some((status, body));
    }

    pub fn last_upstream(&self) -> (String, String, Option<Value>) {
        self.seen.lock().unwrap().requests.last().cloned().unwrap()
    }

    /// Emit a live event on the stub's SSE stream.
    pub fn push_event(&self, ev: Value) {
        let _ = self.live.send(ev);
    }

    pub fn ws_url(&self, path: &str) -> String {
        format!("ws://{}{}", self.addr, path)
    }
}

pub async fn harness() -> Harness {
    harness_with(Options::default()).await
}

pub async fn harness_with(opts: Options) -> Harness {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let rig = Arc::new(Mutex::new(None));
    let (live, _) = tokio::sync::broadcast::channel(64);
    let stub = StubState {
        seen: seen.clone(),
        rig: rig.clone(),
        live: live.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cp_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, stub_router(stub)).await.unwrap();
    });
    let voice = if opts.voice {
        let fl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fish_addr = fl.local_addr().unwrap();
        let fish_seen = seen.clone();
        tokio::spawn(async move {
            axum::serve(fl, fish_router(fish_seen)).await.unwrap();
        });
        Some(
            FishAudio::with_base_url(
                VoiceConfig {
                    fish_audio_api_key: FISH_KEY.into(),
                    voice_id: "voice_test".into(),
                    model: Some("s2.1-pro-free".into()),
                },
                &format!("http://{fish_addr}"),
            )
            .unwrap(),
        )
    } else {
        None
    };
    let db = Db::in_memory().unwrap();
    let control_plane = ControlPlane::new(&format!("http://{cp_addr}"), CP_TOKEN).unwrap();
    let app = opus_api::app(
        AppState {
            db: db.clone(),
            control_plane,
            limiter: Arc::new(RateLimiter::new(opts.rate_limit_per_minute)),
            voice: Arc::new(voice),
        },
        &opts.allowed_origins,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = app.clone();
    tokio::spawn(async move {
        axum::serve(listener, served).await.unwrap();
    });
    Harness {
        app,
        db,
        seen,
        addr,
        rig,
        live,
    }
}

/// One request through the real router; body parsed as JSON when present.
pub async fn call(
    h: &Harness,
    method: Method,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let (status, headers, bytes) = call_raw(h, method, path, key, body).await;
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            panic!(
                "non-JSON body for {status}: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, headers, json)
}

pub async fn call_raw(
    h: &Harness,
    method: Method,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, bytes::Bytes) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(k) = key {
        req = req.header(header::AUTHORIZATION, format!("Bearer {k}"));
    }
    let req = match body {
        Some(b) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = h.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes)
}

pub fn assert_envelope(json: &Value, kind: &str) {
    assert_eq!(json["error"]["type"], kind, "body: {json}");
    assert!(json["error"]["message"].is_string(), "body: {json}");
    assert!(
        json["error"]["request_id"]
            .as_str()
            .map(|s| s.starts_with("req_"))
            .unwrap_or(false),
        "body: {json}"
    );
}

// ---- the stub control plane ------------------------------------------

fn stub_router(state: StubState) -> Router {
    Router::new()
        .route("/agents", get(agents))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/sessions/{id}", get(get_session))
        .route("/sessions/{id}/events", get(events).post(send_event))
        .route("/sessions/{id}/stream", get(stream))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/tool-results", post(tool_results))
        .route("/usage", get(usage))
        .route("/usage/export.csv", get(usage_csv))
        .route("/inference/models", get(models))
        .route("/inference/chat", post(chat))
        .route("/inference/embeddings", post(embeddings))
        .layer(axum::middleware::from_fn_with_state(state.clone(), record))
        .with_state(state)
}

async fn record(
    State(state): State<StubState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {CP_TOKEN}"));
    if !authorized {
        return cp_error(401, "unauthorized", "unauthorized");
    }
    let method = req.method().to_string();
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let (parts, body) = req.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).ok();
    state
        .seen
        .lock()
        .unwrap()
        .requests
        .push((method, path, json));
    state.seen.lock().unwrap().authorized = true;
    next.run(axum::extract::Request::from_parts(parts, Body::from(bytes)))
        .await
}

fn cp_error(status: u16, kind: &str, message: &str) -> Response {
    let mut res = (
        StatusCode::from_u16(status).unwrap(),
        Json(json!({"error": {"type": kind, "message": message}})),
    )
        .into_response();
    if status == 503 {
        res.headers_mut()
            .insert(header::RETRY_AFTER, "5".parse().unwrap());
    }
    res
}

async fn agents() -> Json<Value> {
    Json(json!([
        {"slug":"jarvis","agent_id":"agent_1","agent_version":5,"max_list_cost_cents":"50","effort":"low","default_environment":"cloud-default","synced_at":"2026-09-18T00:00:00Z"},
        {"slug":"gpu-compute","agent_id":"agent_2","agent_version":2,"max_list_cost_cents":"500","effort":"medium","default_environment":"rig-gpu","synced_at":"2026-09-18T00:00:00Z"}
    ]))
}

async fn list_sessions(Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("agent_slug").map(String::as_str) == Some("nope") {
        return cp_error(404, "unknown_agent", "unknown agent slug `nope`");
    }
    Json(json!({"data":[{"id":"sesn_1","status":"idle","console_url":"https://platform.claude.com/x/sesn_1"}],"next_page":null,"prev_page":null})).into_response()
}

async fn create_session(Json(body): Json<Value>) -> Response {
    match body["agent_slug"].as_str() {
        Some("nope") => cp_error(404, "unknown_agent", "unknown agent slug `nope`"),
        Some("gpu-compute") if body["environment"] == "unprovisioned" => cp_error(
            409,
            "environment_not_provisioned",
            "environment `unprovisioned` is defined but not provisioned yet",
        ),
        _ => (
            StatusCode::CREATED,
            Json(json!({
                "session_id":"sesn_new","status":"running","agent_slug":body["agent_slug"],
                "agent_id":"agent_1","agent_version":5,"environment":"cloud-default",
                "environment_id":"env_1","budget":{"max_list_cost_cents":"50"},
                "console_url":"https://platform.claude.com/x/sesn_new"
            })),
        )
            .into_response(),
    }
}

async fn get_session(Path(id): Path<String>) -> Response {
    if id == "sesn_missing" {
        return cp_error(404, "upstream", "not_found_error: session not found");
    }
    if id == "sesn_malformed" {
        // What the live control plane does with Anthropic's 400: renders it 502.
        return cp_error(
            502,
            "upstream",
            "invalid_request_error: Invalid session ID: sesn_malformed",
        );
    }
    if id == "sesn_outage" {
        return cp_error(502, "upstream", "overloaded_error: Overloaded");
    }
    Json(json!({"id": id, "status": "idle", "agent": {"system": "SECRET"}, "console_url": "https://platform.claude.com/x/sesn_1"})).into_response()
}

async fn events(Query(q): Query<HashMap<String, String>>) -> Json<Value> {
    Json(
        json!({"data":[{"id":"sevt_1","type":"agent.message","content":[{"type":"text","text":"hi"}]}],
                "next_page":null,"prev_page":null,"echo_query": q}),
    )
}

async fn send_event(Json(body): Json<Value>) -> Json<Value> {
    Json(
        json!({"data":[{"id":"sevt_2","type":"user.message","content":[{"type":"text","text":body["task"]}]}]}),
    )
}

async fn interrupt() -> Json<Value> {
    Json(json!({"data":[{"id":"sevt_3","type":"user.interrupt"}]}))
}

async fn tool_results(Json(body): Json<Value>) -> Json<Value> {
    let data: Vec<Value> = body["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .enumerate()
        .map(|(i, r)| {
            json!({"id": format!("sevt_tr{i}"), "type": "user.custom_tool_result",
                   "custom_tool_use_id": r["custom_tool_use_id"],
                   "content": [{"type": "text", "text": r["content"]}]})
        })
        .collect();
    Json(json!({"data": data}))
}

/// Two canned frames; for `sesn_live` also whatever tests `push_event`, for
/// as long as the client stays connected — like the real stream. Other ids
/// end after the canned frames so body-collecting tests finish.
async fn stream(
    State(state): State<StubState>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if id == "sesn_missing" {
        return cp_error(404, "upstream", "not_found_error: session not found");
    }
    let rx = state.live.subscribe();
    let endless = id == "sesn_live";
    // With event_deltas, the real stream previews the agent.message as
    // event_start + event_delta frames before the buffered event.
    let preview = if q.get("event_deltas").map(String::as_str) == Some("agent.message") {
        "event: message\ndata: {\"type\":\"event_start\",\"event\":{\"type\":\"agent.message\",\"id\":\"sevt_9\"}}\n\nevent: message\ndata: {\"type\":\"event_delta\",\"event_id\":\"sevt_9\",\"delta\":{\"type\":\"content_delta\",\"index\":0,\"content\":{\"type\":\"text\",\"text\":\"17 times \"}}}\n\nevent: message\ndata: {\"type\":\"event_delta\",\"event_id\":\"sevt_9\",\"delta\":{\"type\":\"content_delta\",\"index\":0,\"content\":{\"type\":\"text\",\"text\":\"23 is 391.\"}}}\n\n"
    } else {
        ""
    };
    let head = format!("{}{}", ": connected\n\n", preview)
        + "event: message\ndata: {\"id\":\"sevt_9\",\"type\":\"agent.message\"}\n\nevent: message\ndata: {\"id\":\"sevt_10\",\"type\":\"session.status_idle\"}\n\n";
    let live = futures_util::stream::unfold((rx, endless), |(mut rx, endless)| async move {
        if !endless {
            return None;
        }
        match rx.recv().await {
            Ok(v) => Some((format!("event: message\ndata: {v}\n\n"), (rx, endless))),
            Err(_) => None,
        }
    });
    let body =
        futures_util::stream::once(
            async move { Ok::<_, std::io::Error>(bytes::Bytes::from(head)) },
        )
        .chain(live.map(|s| Ok(bytes::Bytes::from(s))));
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(body))
        .unwrap()
}

async fn usage(Query(q): Query<HashMap<String, String>>) -> Response {
    if q.get("since").map(String::as_str) == Some("bad") {
        return cp_error(400, "invalid_request", "since: expected RFC 3339");
    }
    Json(json!({"window":{"since":q.get("since"),"until":q.get("until")},
                "by_agent":[{"agent_slug":"jarvis","session_count":9,"total_list_cost_cents":82,"budget_reached_count":0}],
                "recent":[{"session_id":"sesn_1","agent_slug":"jarvis","environment_slug":"cloud-default","list_cost_cents":"5","input_tokens":1,"output_tokens":2,"active_seconds":1.5,"budget_reached":false,"last_event_type":"session.status_idled","observed_at":"2026-09-18T00:00:00Z","last_error":null}]}))
        .into_response()
}

async fn usage_csv() -> Response {
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"session_usage-x.csv\"",
        )
        .body(Body::from("session_id,agent_slug\nsesn_1,jarvis\n"))
        .unwrap()
}

async fn models(State(state): State<StubState>) -> Response {
    if let Some((status, body)) = state.rig.lock().unwrap().clone() {
        let mut res = (StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
        if status == 503 {
            res.headers_mut()
                .insert(header::RETRY_AFTER, "5".parse().unwrap());
        }
        return res;
    }
    Json(json!({"models":[{"name":"qwen3:8b","size":1},{"name":"nomic-embed-text:latest","size":2}]})).into_response()
}

async fn chat(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let _ = headers;
    if body["model"] == "missing" {
        return cp_error(404, "inference", "model 'missing' not found");
    }
    if body["stream"] == false {
        return Json(json!({"model":body["model"],"message":{"role":"assistant","content":"391"},"done":true})).into_response();
    }
    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from("{\"message\":{\"content\":\"3\"},\"done\":false}\n{\"message\":{\"content\":\"91\"},\"done\":true}\n"))
        .unwrap()
}

async fn embeddings(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({"model": body["model"], "embeddings": [[0.1, 0.2, 0.3]]}))
}

// ---- the stub Fish Audio ---------------------------------------------

fn fish_router(seen: Arc<Mutex<Seen>>) -> Router {
    Router::new()
        .route("/v1/tts", post(fish_tts))
        .with_state(seen)
}

async fn fish_tts(
    State(seen): State<Arc<Mutex<Seen>>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let model = headers
        .get("model")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    seen.lock().unwrap().requests.push((
        "POST".into(),
        format!("/v1/tts model={model}"),
        Some(body.clone()),
    ));
    if auth != format!("Bearer {FISH_KEY}") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status":401,"message":"Unauthorized"})),
        )
            .into_response();
    }
    match body["text"].as_str().unwrap_or("") {
        "no credit" => (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({"status":402,"message":"Insufficient API credit."})),
        )
            .into_response(),
        "overloaded" => {
            let mut r = (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status":503,"message":"overloaded"})),
            )
                .into_response();
            r.headers_mut()
                .insert(header::RETRY_AFTER, "3".parse().unwrap());
            r
        }
        _ => {
            let ct = match body["format"].as_str().unwrap_or("mp3") {
                "wav" => "audio/wav",
                "pcm" => "audio/pcm",
                "opus" => "audio/opus",
                _ => "audio/mpeg",
            };
            // Two chunks, like a streamed response.
            let chunks = futures_util::stream::iter(vec![
                Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"ID3fake-mp3-")),
                Ok(bytes::Bytes::from_static(b"audio-bytes")),
            ]);
            Response::builder()
                .status(200)
                .header(header::CONTENT_TYPE, ct)
                .body(Body::from_stream(chunks))
                .unwrap()
        }
    }
}
