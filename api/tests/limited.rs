//! Keys limited to some agents (`--agents`): Mr. Powers's web key reaches
//! only `jarvis-powers`. Every other key behaves as before (the rest of the
//! suite runs with unlimited keys).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;

fn ids(json: &serde_json::Value, field: &str) -> Vec<String> {
    json.as_array()
        .unwrap()
        .iter()
        .map(|r| r[field].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn creates_sessions_only_for_its_agents() {
    let h = harness().await;
    let key = h.limited_key(&["jarvis-powers"]);
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug": "jarvis-powers", "task": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");

    for agent in ["jarvis", "blueweb-client"] {
        let before = h.seen.lock().unwrap().requests.len();
        let (status, _, json) = call(
            &h,
            Method::POST,
            "/v1/sessions",
            Some(&key),
            Some(json!({"agent_slug": agent, "task": "hello"})),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_envelope(&json, "forbidden");
        assert_eq!(
            h.seen.lock().unwrap().requests.len(),
            before,
            "nothing reaches upstream"
        );
    }
}

#[tokio::test]
async fn other_agents_sessions_do_not_exist_for_it() {
    let h = harness().await;
    let key = h.limited_key(&["jarvis-powers"]);
    let reads = [
        (Method::GET, "/v1/sessions/sesn_1", None),
        (Method::GET, "/v1/sessions/sesn_1/events", None),
        (Method::GET, "/v1/sessions/sesn_1/stream", None),
        (Method::GET, "/v1/sessions/sesn_1/files", None),
        (
            Method::POST,
            "/v1/sessions/sesn_1/events",
            Some(json!({"task": "hi"})),
        ),
        (
            Method::POST,
            "/v1/sessions/sesn_1/tool-results",
            Some(json!({"results": [{"custom_tool_use_id": "sevt_9", "content": "x"}]})),
        ),
        (Method::POST, "/v1/sessions/sesn_1/interrupt", None),
    ];
    for (method, path, body) in reads {
        let (status, _, json) = call(&h, method.clone(), path, Some(&key), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}: {json}");
        assert_envelope(&json, "not_found");
        let (m, p, _) = h.last_upstream();
        assert_eq!(
            (m.as_str(), p.as_str()),
            ("GET", "/sessions/sesn_1"),
            "{method} {path}: only the ownership lookup went upstream"
        );
    }
}

#[tokio::test]
async fn its_own_sessions_work_and_the_lookup_is_cached() {
    let h = harness().await;
    let key = h.limited_key(&["jarvis-powers"]);
    let (status, _, json) = call(&h, Method::GET, "/v1/sessions/sesn_pw1", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions/sesn_pw1/events",
        Some(&key),
        Some(json!({"task": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let lookups = h
        .seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|(m, p, _)| m == "GET" && p == "/sessions/sesn_pw1")
        .count();
    assert_eq!(
        lookups, 1,
        "the GET itself recorded the agent; the send reused it"
    );
}

#[tokio::test]
async fn lists_show_only_its_agents() {
    let h = harness().await;
    let key = h.limited_key(&["jarvis-powers"]);

    let (status, _, json) = call(&h, Method::GET, "/v1/sessions", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(ids(&json["data"], "id"), ["sesn_pw1"]);
    let (_, path, _) = h.last_upstream();
    assert_eq!(path, "/sessions?agent_slug=jarvis-powers");

    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions?agent_slug=jarvis",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");

    let (status, _, json) = call(&h, Method::GET, "/v1/fleet/agents", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(ids(&json["data"], "slug"), ["jarvis-powers"]);

    let (status, _, json) = call(&h, Method::GET, "/v1/usage", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(ids(&json["by_agent"], "agent_slug"), ["jarvis-powers"]);
    assert!(json["recent"].as_array().unwrap().is_empty(), "{json}");

    let (status, _, json) = call(&h, Method::GET, "/v1/usage/export.csv", Some(&key), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
}

#[tokio::test]
async fn unlimited_keys_see_everything_as_before() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let (_, _, json) = call(&h, Method::GET, "/v1/sessions", Some(&key), None).await;
    assert_eq!(ids(&json["data"], "id"), ["sesn_1", "sesn_pw1"]);
    let (_, path, _) = h.last_upstream();
    assert_eq!(path, "/sessions");
    let (_, _, json) = call(&h, Method::GET, "/v1/usage", Some(&key), None).await;
    assert_eq!(json["by_agent"].as_array().unwrap().len(), 2);
    let (status, _, _) = call(
        &h,
        Method::GET,
        "/v1/sessions/sesn_1/events",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn files_download_only_from_its_own_sessions() {
    let h = harness().await;
    let key = h.limited_key(&["jarvis-powers"]);
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/files/file_out/content",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a session is required: {json}"
    );

    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/files/file_out/content?session=sesn_1",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");

    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/files/file_other/content?session=sesn_pw1",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "not that session's file: {json}"
    );

    let (status, _, body) = call_raw(
        &h,
        Method::GET,
        "/v1/files/file_out/content?session=sesn_pw1",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"# hi\n");
}

#[tokio::test]
async fn me_is_unchanged_and_admin_can_mint_limited_keys() {
    let h = harness().await;
    let admin = h.key("ops", &[opus_api::auth::keys::Scope::KeysAdmin]);
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/keys",
        Some(&admin),
        Some(json!({"name": "web-powers", "scopes": ["sessions:read"], "agents": ["jarvis-powers", "jarvis-powers"]})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json["agents"], json!(["jarvis-powers"]));
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/keys",
        Some(&admin),
        Some(json!({"name": "bad", "scopes": ["sessions:read"], "agents": ["Jarvis!"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}
