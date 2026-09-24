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
use opus_api::config::{OpsConfig, VoiceConfig};
use opus_api::db::Db;
use opus_api::upstream::control_plane::ControlPlane;
use opus_api::upstream::fish_audio::FishAudio;
use opus_api::upstream::ops::{BaseUrls, Ops};
use opus_api::v1::keys::create_key;
use opus_api::v1::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

pub const CP_TOKEN: &str = "cp-secret";
pub const FISH_KEY: &str = "sk-fish-test";
pub const OPS_TOKEN: &str = "ops-read-token";

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
    /// Register `/v1/ops*` against one stub that plays every service (and
    /// a unix-socket Docker stub). Off = the routes don't exist.
    pub ops: bool,
    /// With `ops`: make the GitHub stub answer 401, to see a `down` row.
    pub github_rejects: bool,
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
    let ops = if opts.ops {
        let ol = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ops_addr = ol.local_addr().unwrap();
        let ops_seen = seen.clone();
        let rejects = opts.github_rejects;
        tokio::spawn(async move {
            axum::serve(ol, ops_router(ops_seen, rejects))
                .await
                .unwrap();
        });
        let sock = std::env::temp_dir().join(format!("opus-api-test-{}.sock", getrandom_u64()));
        let _ = std::fs::remove_file(&sock);
        let ul = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            axum::serve(ul, docker_router()).await.unwrap();
        });
        let base = format!("http://{ops_addr}");
        Some(
            Ops::with_base_urls(
                OpsConfig {
                    uptimerobot_api_key: Some(OPS_TOKEN.into()),
                    tailscale_api_key: Some(OPS_TOKEN.into()),
                    tailscale_tailnet: "-".into(),
                    cloudflare_api_token: Some(OPS_TOKEN.into()),
                    digitalocean_token: Some(OPS_TOKEN.into()),
                    github_token: Some(OPS_TOKEN.into()),
                    github_org: "Opus-Systems-OS".into(),
                    docker_socket: Some(sock),
                },
                BaseUrls {
                    uptimerobot: base.clone(),
                    tailscale: base.clone(),
                    cloudflare: base.clone(),
                    digitalocean: base.clone(),
                    github: base,
                },
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
            ops: Arc::new(ops),
            pairings: Default::default(),
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

/// A raw (non-JSON) request body with its own content type; the response
/// parsed as JSON when it is.
pub async fn call_bytes(
    h: &Harness,
    path: &str,
    key: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {key}"))
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
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

fn getrandom_u64() -> u64 {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    u64::from_le_bytes(b)
}

// ---- the stub services behind /v1/ops -----------------------------------

#[derive(Clone)]
struct OpsStub {
    seen: Arc<Mutex<Seen>>,
    github_rejects: bool,
}

fn ops_router(seen: Arc<Mutex<Seen>>, github_rejects: bool) -> Router {
    Router::new()
        .route("/v2/getMonitors", post(ur_monitors))
        .route("/api/v2/tailnet/{tailnet}/devices", get(ts_devices))
        .route("/client/v4/zones", get(cf_zones))
        .route("/client/v4/zones/{id}/dns_records", get(cf_records))
        .route("/v2/droplets", get(do_droplets))
        .route("/v2/monitoring/metrics/droplet/{metric}", get(do_metric))
        .route("/orgs/{org}/repos", get(gh_repos))
        .route("/repos/{owner}/{repo}/pulls", get(gh_pulls))
        .route("/repos/{owner}/{repo}/actions/runs", get(gh_runs))
        .route("/notifications", get(gh_notifications))
        .layer(axum::middleware::from_fn_with_state(seen, record_ops))
        .with_state(OpsStub {
            seen: Arc::new(Mutex::new(Seen::default())),
            github_rejects,
        })
}

async fn record_ops(
    State(seen): State<Arc<Mutex<Seen>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let bearer_ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {OPS_TOKEN}"))
        .unwrap_or(false);
    seen.lock().unwrap().requests.push((
        format!("ops {}", req.method()),
        format!("{path} bearer_ok={bearer_ok}"),
        None,
    ));
    next.run(req).await
}

async fn ur_monitors(body: String) -> Json<Value> {
    if !body.contains(&format!("api_key={OPS_TOKEN}")) {
        return Json(
            json!({"stat":"fail","error":{"type":"invalid_parameter","message":"api_key is invalid"}}),
        );
    }
    Json(json!({"stat":"ok","monitors":[
        {"friendly_name":"api.opustower.dev","url":"https://api.opustower.dev/v1/health","status":2,"custom_uptime_ratio":"100.000-99.987-99.900","response_times":[{"datetime":1,"value":212}]},
        {"friendly_name":"fleet.opustower.dev","url":"https://fleet.opustower.dev/healthz","status":2,"custom_uptime_ratio":"100.000-100.000-100.000","response_times":[{"datetime":1,"value":180}]}
    ]}))
}

async fn ts_devices() -> Json<Value> {
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    Json(json!({"devices":[
        {"name":"opus.tail1234.ts.net","hostname":"opus","os":"windows","addresses":["100.79.233.8"],"lastSeen":"2026-09-01T00:00:00Z","updateAvailable":false},
        {"name":"opustower.tail1234.ts.net","hostname":"opustower","os":"linux","addresses":["100.108.133.31"],"lastSeen":now,"updateAvailable":true}
    ]}))
}

async fn cf_zones() -> Json<Value> {
    Json(
        json!({"result":[{"id":"z1","name":"opustower.dev","status":"active","paused":false}],"success":true}),
    )
}

async fn cf_records(Path(id): Path<String>) -> Json<Value> {
    assert_eq!(id, "z1");
    Json(json!({"result":[
        {"type":"A","name":"api.opustower.dev","content":"198.199.66.109","proxied":true,"ttl":1},
        {"type":"A","name":"fleet.opustower.dev","content":"198.199.66.109","proxied":true,"ttl":1},
        {"type":"AAAA","name":"mcp.opustower.dev","content":"2604:a880:400:d1:0:4:f807:7001","proxied":false,"ttl":300}
    ],"success":true}))
}

async fn do_droplets() -> Json<Value> {
    Json(
        json!({"droplets":[{"id":4242,"name":"opustower","status":"active","region":{"slug":"nyc1"},"vcpus":1,"memory":1024,"disk":25,"created_at":"2026-09-14T00:00:00Z",
        "networks":{"v4":[{"ip_address":"10.0.0.2","type":"private"},{"ip_address":"198.199.66.109","type":"public"}]}}]}),
    )
}

async fn do_metric(Path(metric): Path<String>) -> Json<Value> {
    let v = match metric.as_str() {
        "load_1" => "0.12",
        "memory_total" => "1024000000",
        "memory_available" => "600000000",
        _ => "0",
    };
    Json(json!({"status":"success","data":{"result":[{"metric":{},"values":[[1,"0"],[2,v]]}]}}))
}

async fn gh_repos(State(s): State<OpsStub>, Path(org): Path<String>) -> Response {
    if s.github_rejects {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"message":"Bad credentials"})),
        )
            .into_response();
    }
    assert_eq!(org, "Opus-Systems-OS");
    Json(json!([
        {"name":"Iron-Fleet","full_name":"Opus-Systems-OS/Iron-Fleet","private":true,"pushed_at":"2026-09-19T20:00:00Z","default_branch":"main"},
        {"name":"Opus-Systems-OS-API","full_name":"Opus-Systems-OS/Opus-Systems-OS-API","private":true,"pushed_at":"2026-09-19T19:00:00Z","default_branch":"main"}
    ])).into_response()
}

async fn gh_pulls(Path((_, repo)): Path<(String, String)>) -> Json<Value> {
    if repo == "Iron-Fleet" {
        Json(
            json!([{"number":36,"title":"control-plane: client label on sessions","user":{"login":"jameswalker"},"draft":false,"updated_at":"2026-09-19T20:00:00Z","html_url":"https://github.com/Opus-Systems-OS/Iron-Fleet/pull/36"}]),
        )
    } else {
        Json(json!([]))
    }
}

async fn gh_runs(Path((_, repo)): Path<(String, String)>) -> Json<Value> {
    let conclusion = if repo == "Iron-Fleet" {
        "failure"
    } else {
        "success"
    };
    Json(
        json!({"workflow_runs":[{"name":"images","status":"completed","conclusion":conclusion,"head_branch":"main","updated_at":"2026-09-19T20:05:00Z","html_url":"https://github.com/x/y/actions/runs/1"}]}),
    )
}

async fn gh_notifications() -> Json<Value> {
    Json(
        json!([{"subject":{"title":"control-plane: client label on sessions","type":"PullRequest"},"repository":{"full_name":"Opus-Systems-OS/Iron-Fleet"},"reason":"review_requested","updated_at":"2026-09-19T20:00:00Z"}]),
    )
}

fn docker_router() -> Router {
    Router::new().route("/containers/json", get(docker_containers))
}

async fn docker_containers() -> Json<Value> {
    Json(json!([
        {"Names":["/droplet-api-1"],"Image":"ghcr.io/opus-systems-os/opus-systems-os-api/api:latest","State":"running","Status":"Up 3 hours","Ports":[{"PrivatePort":8100,"Type":"tcp"}]},
        {"Names":["/droplet-caddy-1"],"Image":"caddy:2","State":"running","Status":"Up 3 days","Ports":[{"PrivatePort":443,"PublicPort":443,"Type":"tcp"}]},
        {"Names":["/droplet-old-1"],"Image":"x","State":"exited","Status":"Exited (0) 2 days ago","Ports":[]}
    ]))
}

// ---- the stub Fish Audio ---------------------------------------------

fn fish_router(seen: Arc<Mutex<Seen>>) -> Router {
    Router::new()
        .route("/v1/tts", post(fish_tts))
        .route("/v1/asr", post(fish_asr))
        .route("/wallet/self/api-credit", get(fish_credit))
        .with_state(seen)
}

async fn fish_credit(State(seen): State<Arc<Mutex<Seen>>>, headers: HeaderMap) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    seen.lock()
        .unwrap()
        .requests
        .push(("GET".into(), "/wallet/self/api-credit".into(), None));
    if auth != format!("Bearer {FISH_KEY}") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status":401,"message":"Unauthorized"})),
        )
            .into_response();
    }
    Json(json!({
        "_id": "wallet_1",
        "user_id": "fish-user-secret",
        "credit": "12.34",
        "cumulative_top_up": "20.00",
        "created_at": "2026-09-01T00:00:00Z",
        "updated_at": "2026-09-24T00:00:00Z",
        "has_phone_sha256": false
    }))
    .into_response()
}

/// Multipart in, `{text, duration, …}` out. The raw body is inspected rather
/// than parsed: the test only needs to know the parts arrived.
async fn fish_asr(
    State(seen): State<Arc<Mutex<Seen>>>,
    headers: HeaderMap,
    body: bytes::Bytes,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let text = String::from_utf8_lossy(&body).into_owned();
    seen.lock().unwrap().requests.push((
        "POST".into(),
        "/v1/asr".into(),
        Some(json!({
            "content_type": ct,
            "has_audio_part": text.contains("name=\"audio\""),
            "audio_mime": text.contains("Content-Type: audio/wav"),
            "language_en": text.contains("name=\"language\"\r\n\r\nen"),
        })),
    ));
    if auth != format!("Bearer {FISH_KEY}") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"status":401,"message":"Unauthorized"})),
        )
            .into_response();
    }
    if text.contains("garbage") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"status":400,"message":"Invalid audio input: the audio could not be decoded (format not recognised)."})),
        )
            .into_response();
    }
    if text.contains("no credit") {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({"status":402,"message":"Insufficient API credit."})),
        )
            .into_response();
    }
    Json(json!({
        "text": "Jarvis, what is the weather in Calabasas today?",
        "duration": 3.2,
        "segments": [],
        "language": "en"
    }))
    .into_response()
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
