//! Stage 1 contract tests: auth, scopes, the error envelope, request ids,
//! the spec. Everything goes through the real router with an in-memory DB.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use opus_api::auth::keys::Scope;
use opus_api::db::Db;
use opus_api::v1::keys::create_key;
use opus_api::v1::AppState;
use serde_json::Value;
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    db: Db,
}

fn harness() -> Harness {
    let db = Db::in_memory().unwrap();
    let app = opus_api::app(AppState { db: db.clone() });
    Harness { app, db }
}

async fn call(
    h: &Harness,
    method: Method,
    path: &str,
    key: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, http::HeaderMap, Value) {
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

fn assert_envelope(json: &Value, kind: &str) {
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

#[tokio::test]
async fn health_is_public_and_versioned() {
    let h = harness();
    let (status, headers, json) = call(&h, Method::GET, "/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
    assert!(headers.get("x-request-id").is_some());
}

#[tokio::test]
async fn me_needs_a_valid_key() {
    let h = harness();
    let (status, _, json) = call(&h, Method::GET, "/v1/me", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_envelope(&json, "unauthorized");

    let (status, _, json) = call(&h, Method::GET, "/v1/me", Some("osk_not_a_key"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_envelope(&json, "unauthorized");

    // Well-formed but unknown id.
    let fake = format!("osk_{}_{}", "0".repeat(8), "f".repeat(64));
    let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&fake), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let created = create_key(&h.db, "mac", &[Scope::FleetRead, Scope::SessionsRead]).unwrap();
    let (status, _, json) = call(&h, Method::GET, "/v1/me", Some(&created.key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["key_id"], created.id);
    assert_eq!(json["name"], "mac");
    assert_eq!(
        json["scopes"],
        serde_json::json!(["fleet:read", "sessions:read"])
    );

    // Right id, wrong secret.
    let wrong = format!("osk_{}_{}", created.id, "0".repeat(64));
    let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&wrong), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Revoked.
    assert!(h.db.revoke_key(&created.id).unwrap());
    let (status, _, json) = call(&h, Method::GET, "/v1/me", Some(&created.key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_envelope(&json, "unauthorized");
}

#[tokio::test]
async fn keys_admin_scope_gates_key_management() {
    let h = harness();
    let device = create_key(&h.db, "quest-3", &[Scope::SessionsWrite]).unwrap();
    let admin = create_key(&h.db, "ops", &[Scope::KeysAdmin]).unwrap();

    let (status, _, json) = call(&h, Method::GET, "/v1/keys", Some(&device.key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&json, "forbidden");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("keys:admin"));

    let (status, _, json) = call(&h, Method::GET, "/v1/keys", Some(&admin.key), None).await;
    assert_eq!(status, StatusCode::OK);
    let list = json["data"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert!(
        list.iter().all(|k| k.get("secret_sha256").is_none()),
        "listing never shows hashes: {json}"
    );

    // Create over HTTP, then use the returned key.
    let (status, _, created) = call(
        &h,
        Method::POST,
        "/v1/keys",
        Some(&admin.key),
        Some(serde_json::json!({"name": "jarvis-ios", "scopes": ["sessions:read", "sessions:write"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let new_key = created["key"].as_str().unwrap().to_owned();
    assert!(new_key.starts_with("osk_"));
    let (status, _, me) = call(&h, Method::GET, "/v1/me", Some(&new_key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["name"], "jarvis-ios");

    // keys:admin cannot be granted over HTTP.
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/keys",
        Some(&admin.key),
        Some(serde_json::json!({"name": "evil", "scopes": ["keys:admin"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");

    // Revoke, then it stops working; a second revoke is 404.
    let id = created["id"].as_str().unwrap();
    let (status, _, _) = call(
        &h,
        Method::DELETE,
        &format!("/v1/keys/{id}"),
        Some(&admin.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&new_key), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, json) = call(
        &h,
        Method::DELETE,
        &format!("/v1/keys/{id}"),
        Some(&admin.key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");
}

#[tokio::test]
async fn axum_rejections_wear_the_envelope_too() {
    let h = harness();
    let admin = create_key(&h.db, "ops", &[Scope::KeysAdmin]).unwrap();

    // Unknown route.
    let (status, _, json) = call(&h, Method::GET, "/v1/nothing", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");

    // Wrong method on a known route.
    let (status, _, json) = call(&h, Method::DELETE, "/v1/health", None, None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_envelope(&json, "method_not_allowed");

    // Malformed JSON body.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/v1/keys")
        .header(header::AUTHORIZATION, format!("Bearer {}", admin.key))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("JSON envelope");
    assert!(status.is_client_error(), "{status}");
    assert_envelope(&json, "invalid_request");

    // Unknown field is rejected (deny_unknown_fields).
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/keys",
        Some(&admin.key),
        Some(serde_json::json!({"name": "x", "scopes": ["inference"], "budget": 1})),
    )
    .await;
    assert!(status.is_client_error(), "{status}");
    assert_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn request_id_is_honoured_when_sane_and_replaced_otherwise() {
    let h = harness();
    let req = Request::builder()
        .uri("/v1/health")
        .header("x-request-id", "quest-42_abc")
        .body(Body::empty())
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.headers()["x-request-id"], "quest-42_abc");

    let req = Request::builder()
        .uri("/v1/nothing")
        .header("x-request-id", "has spaces; and=bad")
        .body(Body::empty())
        .unwrap();
    let res = h.app.clone().oneshot(req).await.unwrap();
    let id = res.headers()["x-request-id"].to_str().unwrap().to_owned();
    assert!(id.starts_with("req_"), "{id}");
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["error"]["request_id"], id,
        "body id matches the header"
    );
}

#[tokio::test]
async fn openapi_lists_every_route_with_security() {
    let h = harness();
    let (status, _, spec) = call(&h, Method::GET, "/v1/openapi.json", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(spec["openapi"].as_str().unwrap()[..2], *"3.");
    let paths = spec["paths"].as_object().unwrap();
    for p in ["/health", "/me", "/keys", "/keys/{id}"] {
        assert!(
            paths.contains_key(p),
            "spec missing {p}: {:?}",
            paths.keys()
        );
    }
    assert!(spec["components"]["securitySchemes"]["api_key"].is_object());
    assert_eq!(
        spec["paths"]["/me"]["get"]["security"][0]["api_key"],
        serde_json::json!([])
    );
    assert!(spec["components"]["schemas"]["ErrorBody"].is_object());
}
