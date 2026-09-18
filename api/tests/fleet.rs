//! Stage 2: the fleet through the door. Every proxied route, its scope, the
//! upstream request it produces, and how upstream errors map.

mod common;

use axum::http::{Method, StatusCode};
use common::{assert_envelope, call, call_raw, harness};
use opus_api::auth::keys::Scope;
use serde_json::json;

#[tokio::test]
async fn agents_are_wrapped_in_data_and_need_fleet_read() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let (status, _, json) = call(&h, Method::GET, "/v1/fleet/agents", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"][0]["slug"], "jarvis");
    assert_eq!(json["data"].as_array().unwrap().len(), 2);
    let (method, path, _) = h.last_upstream();
    assert_eq!((method.as_str(), path.as_str()), ("GET", "/agents"));

    let narrow = h.key("narrow", &[Scope::SessionsRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/fleet/agents", Some(&narrow), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&json, "forbidden");
}

#[tokio::test]
async fn upstream_never_sees_the_client_key() {
    let h = harness().await;
    let key = h.all_scopes_key();
    call(&h, Method::GET, "/v1/fleet/agents", Some(&key), None).await;
    // The stub 401s anything that is not CP_TOKEN, so a 200 above already
    // proves the bearer was swapped; make it explicit.
    assert!(h.seen.lock().unwrap().authorized);
}

#[tokio::test]
async fn sessions_create_list_get_events_send_interrupt() {
    let h = harness().await;
    let key = h.all_scopes_key();

    let (status, _, created) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug": "jarvis", "task": "Hey Jarvis"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["session_id"], "sesn_new");
    assert_eq!(created["budget"]["max_list_cost_cents"], "50");
    let (_, _, body) = h.last_upstream();
    assert_eq!(
        body.unwrap(),
        json!({"agent_slug": "jarvis", "task": "Hey Jarvis"})
    );

    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions?agent_slug=jarvis&limit=5&order=desc",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"][0]["id"], "sesn_1");
    let (_, path, _) = h.last_upstream();
    assert!(path.starts_with("/sessions?"), "{path}");
    assert!(
        path.contains("agent_slug=jarvis")
            && path.contains("limit=5")
            && path.contains("order=desc"),
        "{path}"
    );

    let (status, _, json) = call(&h, Method::GET, "/v1/sessions/sesn_1", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["id"], "sesn_1");

    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions/sesn_1/events?order=desc&types=agent.message,session.error&limit=1",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"][0]["type"], "agent.message");
    assert_eq!(json["echo_query"]["types"], "agent.message,session.error");

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions/sesn_1/events",
        Some(&key),
        Some(json!({"task": "and the driver version?"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"][0]["type"], "user.message");

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions/sesn_1/interrupt",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["data"][0]["type"], "user.interrupt");
}

#[tokio::test]
async fn session_scopes_split_reads_from_writes() {
    let h = harness().await;
    let reader = h.key("reader", &[Scope::SessionsRead]);
    let writer = h.key("writer", &[Scope::SessionsWrite]);

    let (status, _, _) = call(&h, Method::GET, "/v1/sessions", Some(&reader), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, json) = call(&h, Method::GET, "/v1/sessions", Some(&writer), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sessions:read"));

    let body = json!({"agent_slug": "jarvis", "task": "x"});
    let (status, _, _) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&writer),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, json) = call(&h, Method::POST, "/v1/sessions", Some(&reader), Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sessions:write"));
}

#[tokio::test]
async fn session_validation_happens_before_upstream() {
    let h = harness().await;
    let key = h.all_scopes_key();

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug": "jarvis", "task": "   "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");

    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions/..%2Fagents",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_envelope(&json, "invalid_request");

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug": "jarvis", "task": "x", "budget": "9999"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown fields are rejected: {json}"
    );
    assert!(
        h.seen.lock().unwrap().requests.is_empty(),
        "nothing reached upstream"
    );
}

#[tokio::test]
async fn upstream_errors_map_to_our_vocabulary() {
    let h = harness().await;
    let key = h.all_scopes_key();

    // unknown_agent 404 → not_found 404
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug": "nope", "task": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");
    assert!(json["error"]["message"].as_str().unwrap().contains("nope"));

    // environment_not_provisioned 409 → conflict 409
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug": "gpu-compute", "task": "x", "environment": "unprovisioned"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_envelope(&json, "conflict");

    // Anthropic's own 404 through the control plane → not_found
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions/sesn_missing",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");

    // Anthropic's own 400 (rendered 502 by the control plane) → invalid_request 400
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions/sesn_malformed",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_envelope(&json, "invalid_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Invalid session ID"));

    // A genuine upstream failure stays 502 upstream
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions/sesn_outage",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_envelope(&json, "upstream");

    // invalid_request 400 → invalid_request 400
    let (status, _, json) = call(&h, Method::GET, "/v1/usage?since=bad", Some(&key), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn stream_is_sse_passthrough() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let (status, headers, bytes) = call_raw(
        &h,
        Method::GET,
        "/v1/sessions/sesn_1/stream?event_deltas=agent.message",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/event-stream");
    assert_eq!(headers["x-accel-buffering"], "no");
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.starts_with(": connected\n\n"), "{text}");
    // event_deltas requested: the upstream previews (event_start + two
    // event_delta frames) pass through untouched ahead of the two events.
    assert_eq!(text.matches("event: message\n").count(), 5, "{text}");
    assert!(text.contains("\"type\":\"event_delta\""));
    assert!(text.contains("\"id\":\"sevt_10\""));
    let (_, path, _) = h.last_upstream();
    assert_eq!(path, "/sessions/sesn_1/stream?event_deltas=agent.message");
}

#[tokio::test]
async fn usage_json_and_csv() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/usage?since=2026-09-01&until=2026-10-01",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["by_agent"][0]["agent_slug"], "jarvis");
    assert_eq!(json["window"]["since"], "2026-09-01");
    assert!(json["recent"][0].get("last_error").is_some());

    let (status, headers, bytes) =
        call_raw(&h, Method::GET, "/v1/usage/export.csv", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/csv"));
    assert!(headers.get("content-disposition").is_some());
    assert!(String::from_utf8_lossy(&bytes).starts_with("session_id,agent_slug\n"));

    let narrow = h.key("narrow", &[Scope::FleetRead]);
    let (status, _, _) = call(&h, Method::GET, "/v1/usage", Some(&narrow), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn inference_models_chat_embeddings_and_rig_offline() {
    let h = harness().await;
    let key = h.all_scopes_key();

    let (status, _, json) = call(&h, Method::GET, "/v1/inference/models", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["models"][0]["name"], "qwen3:8b");

    // Buffered chat.
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/inference/chat",
        Some(&key),
        Some(json!({"model": "qwen3:8b", "messages": [{"role": "user", "content": "17*23"}], "stream": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["message"]["content"], "391");

    // Streamed chat: NDJSON passthrough.
    let (status, headers, bytes) = call_raw(
        &h,
        Method::POST,
        "/v1/inference/chat",
        Some(&key),
        Some(json!({"model": "qwen3:8b", "messages": [{"role": "user", "content": "17*23"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/x-ndjson");
    assert_eq!(String::from_utf8_lossy(&bytes).lines().count(), 2);

    // Shape check stops a bad body here.
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/inference/chat",
        Some(&key),
        Some(json!({"model": "qwen3:8b"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("messages"));

    // Ollama's own 404 (unknown model) is the caller's problem.
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/inference/chat",
        Some(&key),
        Some(json!({"model": "missing", "messages": [], "stream": false})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "type and status agree: {json}"
    );
    assert_envelope(&json, "invalid_request");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("missing"));

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/inference/embeddings",
        Some(&key),
        Some(json!({"model": "nomic-embed-text", "input": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["embeddings"][0].as_array().unwrap().len(), 3);

    // Rig off: 503 rig_offline with Retry-After, from any inference route.
    h.rig_answers(503, json!({"error": {"type": "rig_offline", "message": "rig offline: 100.79.233.8:11434: connect error"}}));
    let (status, headers, json) =
        call(&h, Method::GET, "/v1/inference/models", Some(&key), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_envelope(&json, "rig_offline");
    assert_eq!(headers["retry-after"], "5");
}

#[tokio::test]
async fn rig_status_is_three_way() {
    let h = harness().await;
    let key = h.all_scopes_key();

    let (status, _, json) = call(&h, Method::GET, "/v1/rig", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        json!({"configured": true, "online": true, "models": ["qwen3:8b", "nomic-embed-text:latest"], "reason": null})
    );

    h.rig_answers(
        503,
        json!({"error": {"type": "rig_offline", "message": "rig offline: deadline has elapsed"}}),
    );
    let (status, _, json) = call(&h, Method::GET, "/v1/rig", Some(&key), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "offline is a 200 with online:false, not an error"
    );
    assert_eq!(json["online"], false);
    assert_eq!(json["configured"], true);
    assert!(json["reason"].as_str().unwrap().contains("deadline"));

    h.rig_answers(
        404,
        json!({"error": {"type": "not_found", "message": "no route"}}),
    );
    let (status, _, json) = call(&h, Method::GET, "/v1/rig", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["configured"], false);

    // fleet:read, not inference — a dashboard key can see the rig line
    // without being able to run models.
    let dash = h.key("dash", &[Scope::FleetRead]);
    let (status, _, _) = call(&h, Method::GET, "/v1/rig", Some(&dash), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(&h, Method::GET, "/v1/inference/models", Some(&dash), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn spec_covers_stage_2() {
    let h = harness().await;
    let (_, _, spec) = call(&h, Method::GET, "/v1/openapi.json", None, None).await;
    let paths = spec["paths"].as_object().unwrap();
    for p in [
        "/fleet/agents",
        "/rig",
        "/sessions",
        "/sessions/{id}",
        "/sessions/{id}/events",
        "/sessions/{id}/stream",
        "/sessions/{id}/interrupt",
        "/usage",
        "/usage/export.csv",
        "/inference/models",
        "/inference/chat",
        "/inference/embeddings",
    ] {
        assert!(paths.contains_key(p), "spec missing {p}");
    }
    assert_eq!(
        spec["paths"]["/sessions"]["post"]["security"][0]["api_key"][0],
        "sessions:write"
    );
    assert_eq!(
        spec["paths"]["/sessions"]["get"]["security"][0]["api_key"][0],
        "sessions:read"
    );
}

#[tokio::test]
async fn custom_tools_and_system_suffix_reach_upstream_verbatim() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let body = json!({
        "agent_slug": "jarvis",
        "task": "play something",
        "tools": [{"type": "custom", "name": "play_music", "description": "Play music on this device.",
                   "input_schema": {"type": "object", "properties": {"title": {"type": "string"}}}}],
        "system_suffix": "Call me Sir."
    });
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let (_, _, sent) = h.last_upstream();
    let sent = sent.unwrap();
    assert_eq!(sent["tools"][0]["name"], "play_music");
    assert_eq!(
        sent["tools"][0]["input_schema"]["properties"]["title"]["type"],
        "string"
    );
    assert_eq!(sent["system_suffix"], "Call me Sir.");

    // Validation happens here, before any round trip.
    let n = h.seen.lock().unwrap().requests.len();
    for bad in [
        json!([{"type": "agent_toolset_20260401", "name": "x", "description": "d", "input_schema": {}}]),
        json!([{"type": "custom", "name": "Play Music", "description": "d", "input_schema": {}}]),
        json!([{"type": "custom", "name": "x", "description": "", "input_schema": {}}]),
        json!([{"type": "custom", "name": "x", "description": "d", "input_schema": "nope"}]),
    ] {
        let (status, _, json) = call(
            &h,
            Method::POST,
            "/v1/sessions",
            Some(&key),
            Some(json!({"agent_slug": "jarvis", "task": "x", "tools": bad})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert_envelope(&json, "invalid_request");
    }
    assert_eq!(
        h.seen.lock().unwrap().requests.len(),
        n,
        "nothing reached upstream"
    );
}

#[tokio::test]
async fn tool_results_route() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions/sesn_1/tool-results",
        Some(&key),
        Some(json!({"results": [{"custom_tool_use_id": "sevt_42", "content": "Now playing: Around the World"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["data"][0]["type"], "user.custom_tool_result");
    assert_eq!(json["data"][0]["custom_tool_use_id"], "sevt_42");
    let (method, path, body) = h.last_upstream();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/sessions/sesn_1/tool-results")
    );
    assert_eq!(
        body.unwrap()["results"][0]["content"],
        "Now playing: Around the World"
    );

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions/sesn_1/tool-results",
        Some(&key),
        Some(json!({"results": []})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");

    let reader = h.key("reader", &[Scope::SessionsRead]);
    let (status, _, _) = call(
        &h,
        Method::POST,
        "/v1/sessions/sesn_1/tool-results",
        Some(&reader),
        Some(json!({"results": [{"custom_tool_use_id": "sevt_42", "content": "x"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
