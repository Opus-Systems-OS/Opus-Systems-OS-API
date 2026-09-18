//! Stage 3: the WebSocket session channel, per-key rate limits, CORS.

mod common;

use axum::http::{Method, StatusCode};
use common::{assert_envelope, call, harness, harness_with, Options};
use futures_util::{SinkExt, StreamExt};
use opus_api::auth::keys::Scope;
use serde_json::{json, Value};
use std::time::Duration;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header;
use tokio_tungstenite::tungstenite::Message;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect(
    url: &str,
    key: Option<&str>,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut req = url.into_client_request().unwrap();
    if let Some(k) = key {
        req.headers_mut().insert(
            header::AUTHORIZATION,
            format!("Bearer {k}").parse().unwrap(),
        );
    }
    tokio_tungstenite::connect_async(req).await.map(|(s, _)| s)
}

async fn next_json(ws: &mut Socket) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("frame within 5s")
        .expect("socket open")
        .expect("frame ok");
    match msg {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("unexpected frame {other:?}"),
    }
}

async fn send_json(ws: &mut Socket, v: Value) {
    ws.send(Message::Text(v.to_string().into())).await.unwrap();
}

#[tokio::test]
async fn websocket_hello_history_live_send_interrupt() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let mut ws = connect(&h.ws_url("/v1/sessions/sesn_live/ws"), Some(&key))
        .await
        .expect("upgrade");

    let hello = next_json(&mut ws).await;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["session_id"], "sesn_live");
    assert!(hello["request_id"].as_str().unwrap().starts_with("req_"));

    // History: the stub's /events answers one event (sevt_1).
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["type"], "event");
    assert_eq!(ev["event"]["id"], "sevt_1");

    // Then the stream's canned live frames (sevt_9, sevt_10).
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"]["id"], "sevt_9");
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"]["id"], "sevt_10");
    assert_eq!(ev["event"]["type"], "session.status_idle");

    // A live event pushed now arrives; a duplicate of history does not.
    h.push_event(json!({"id": "sevt_1", "type": "agent.message"}));
    h.push_event(json!({"id": "sevt_11", "type": "agent.message", "content": [{"type": "text", "text": "hi"}]}));
    let ev = next_json(&mut ws).await;
    assert_eq!(
        ev["event"]["id"], "sevt_11",
        "sevt_1 was deduplicated: {ev}"
    );

    // Client frames.
    send_json(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut ws).await["type"], "pong");

    send_json(
        &mut ws,
        json!({"type": "message", "task": "and the driver version?"}),
    )
    .await;
    let sent = next_json(&mut ws).await;
    assert_eq!(sent["type"], "sent", "{sent}");
    assert_eq!(sent["data"][0]["type"], "user.message");
    let (method, path, body) = h.last_upstream();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/sessions/sesn_live/events")
    );
    assert_eq!(body.unwrap()["task"], "and the driver version?");

    send_json(&mut ws, json!({"type": "interrupt"})).await;
    let sent = next_json(&mut ws).await;
    assert_eq!(sent["type"], "sent");
    assert_eq!(sent["data"][0]["type"], "user.interrupt");

    // Bad frames get an error frame and the socket stays open.
    send_json(&mut ws, json!({"type": "message", "task": ""})).await;
    let err = next_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "invalid_request");
    ws.send(Message::Text("not json".into())).await.unwrap();
    assert_eq!(next_json(&mut ws).await["error"]["type"], "invalid_request");
    send_json(&mut ws, json!({"type": "teleport"})).await;
    assert_eq!(next_json(&mut ws).await["error"]["type"], "invalid_request");
    send_json(&mut ws, json!({"type": "ping"})).await;
    assert_eq!(next_json(&mut ws).await["type"], "pong", "still open");

    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn websocket_history_can_be_skipped() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let mut ws = connect(
        &h.ws_url("/v1/sessions/sesn_live/ws?history=false"),
        Some(&key),
    )
    .await
    .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"]["id"], "sevt_9", "straight to live: {ev}");
    assert!(
        !h.seen
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|(_, p, _)| p.contains("/events")),
        "history was not listed"
    );
}

#[tokio::test]
async fn websocket_scopes_and_auth() {
    let h = harness().await;

    // No key: the upgrade itself is refused with the envelope.
    let err = connect(&h.ws_url("/v1/sessions/sesn_live/ws"), None)
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("401"), "{err}");

    // Read-only key connects but cannot write.
    let reader = h.key("reader", &[Scope::SessionsRead]);
    let mut ws = connect(
        &h.ws_url("/v1/sessions/sesn_live/ws?history=false"),
        Some(&reader),
    )
    .await
    .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");
    send_json(&mut ws, json!({"type": "message", "task": "x"})).await;
    // Skip live canned frames until the error arrives.
    let mut got = None;
    for _ in 0..5 {
        let f = next_json(&mut ws).await;
        if f["type"] == "error" {
            got = Some(f);
            break;
        }
    }
    let err = got.expect("error frame");
    assert_eq!(err["error"]["type"], "forbidden");
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sessions:write"));

    // Write-only key cannot connect at all.
    let writer = h.key("writer", &[Scope::SessionsWrite]);
    let err = connect(&h.ws_url("/v1/sessions/sesn_live/ws"), Some(&writer))
        .await
        .expect_err("refused");
    assert!(err.to_string().contains("403"), "{err}");
}

#[tokio::test]
async fn websocket_unknown_session_is_a_clean_404() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let err = connect(&h.ws_url("/v1/sessions/sesn_missing/ws"), Some(&key))
        .await
        .expect_err("refused before upgrade");
    assert!(err.to_string().contains("404"), "{err}");
    // A plain GET without the upgrade headers is a 400 — in the envelope,
    // like every other axum rejection.
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/sessions/sesn_live/ws",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn websocket_closes_when_upstream_ends() {
    let h = harness().await;
    let key = h.all_scopes_key();
    // sesn_1's stub stream ends after its canned frames.
    let mut ws = connect(
        &h.ws_url("/v1/sessions/sesn_1/ws?history=false"),
        Some(&key),
    )
    .await
    .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");
    let mut last = Value::Null;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .unwrap()
        {
            Some(Ok(Message::Text(t))) => last = serde_json::from_str(&t).unwrap(),
            Some(Ok(Message::Close(_))) | None => break,
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("{e}"),
        }
    }
    assert_eq!(last["type"], "closed");
    assert_eq!(last["reason"], "upstream_closed");
}

#[tokio::test]
async fn rate_limit_is_per_key_with_retry_after() {
    let h = harness_with(Options {
        rate_limit_per_minute: 5,
        ..Options::default()
    })
    .await;
    let a = h.key("a", &[Scope::FleetRead]);
    let b = h.key("b", &[Scope::FleetRead]);
    for _ in 0..5 {
        let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&a), None).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, headers, json) = call(&h, Method::GET, "/v1/me", Some(&a), None).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_envelope(&json, "rate_limited");
    assert!(headers.get("retry-after").is_some());
    let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&b), None).await;
    assert_eq!(status, StatusCode::OK, "another key is unaffected");
    let (status, _, _) = call(&h, Method::GET, "/v1/health", None, None).await;
    assert_eq!(status, StatusCode::OK, "public routes are not limited");
}

#[tokio::test]
async fn cors_only_for_listed_origins() {
    let h = harness_with(Options {
        allowed_origins: vec!["https://quest.example".into()],
        ..Options::default()
    })
    .await;
    let client = reqwest::Client::new();
    let base = format!("http://{}", h.addr);

    let res = client
        .request(Method::OPTIONS, format!("{base}/v1/me"))
        .header("origin", "https://quest.example")
        .header("access-control-request-method", "GET")
        .header("access-control-request-headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.headers()["access-control-allow-origin"],
        "https://quest.example"
    );
    assert!(res.headers()["access-control-allow-headers"]
        .to_str()
        .unwrap()
        .contains("authorization"));

    let res = client
        .get(format!("{base}/v1/health"))
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());

    let res = client
        .get(format!("{base}/v1/health"))
        .header("origin", "https://quest.example")
        .send()
        .await
        .unwrap();
    assert_eq!(
        res.headers()["access-control-allow-origin"],
        "https://quest.example"
    );
    assert!(res.headers()["access-control-expose-headers"]
        .to_str()
        .unwrap()
        .contains("x-request-id"));

    // No origins configured → no CORS headers at all.
    let plain = harness().await;
    let res = client
        .get(format!("http://{}/v1/health", plain.addr))
        .header("origin", "https://quest.example")
        .send()
        .await
        .unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn websocket_tool_result_frame_and_deltas() {
    let h = harness().await;
    let key = h.all_scopes_key();
    let mut ws = connect(
        &h.ws_url("/v1/sessions/sesn_live/ws?history=false&deltas=true"),
        Some(&key),
    )
    .await
    .unwrap();
    assert_eq!(next_json(&mut ws).await["type"], "hello");

    // With deltas on, the text fragments arrive as `delta` frames before
    // the buffered agent.message event; event_start is dropped.
    let d1 = next_json(&mut ws).await;
    assert_eq!(d1["type"], "delta", "{d1}");
    assert_eq!(d1["event_id"], "sevt_9");
    assert_eq!(d1["text"], "17 times ");
    let d2 = next_json(&mut ws).await;
    assert_eq!(d2["text"], "23 is 391.");
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["type"], "event");
    assert_eq!(
        ev["event"]["id"], "sevt_9",
        "the authoritative event follows: {ev}"
    );
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"]["id"], "sevt_10");

    // A custom tool call arrives as a plain event; the client answers with
    // a tool_result frame.
    h.push_event(json!({"id": "sevt_77", "type": "agent.custom_tool_use", "name": "play_music", "input": {"artist": "Daft Punk"}}));
    let ev = next_json(&mut ws).await;
    assert_eq!(ev["event"]["type"], "agent.custom_tool_use");
    assert_eq!(ev["event"]["input"]["artist"], "Daft Punk");
    send_json(
        &mut ws,
        json!({"type": "tool_result", "custom_tool_use_id": "sevt_77", "content": "Now playing: Around the World"}),
    )
    .await;
    let sent = next_json(&mut ws).await;
    assert_eq!(sent["type"], "sent", "{sent}");
    assert_eq!(sent["data"][0]["type"], "user.custom_tool_result");
    let (method, path, body) = h.last_upstream();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/sessions/sesn_live/tool-results")
    );
    assert_eq!(body.unwrap()["results"][0]["custom_tool_use_id"], "sevt_77");

    // Malformed tool_result is an error frame, socket stays open.
    send_json(&mut ws, json!({"type": "tool_result", "content": "x"})).await;
    let err = next_json(&mut ws).await;
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "invalid_request");
    ws.close(None).await.unwrap();
}
