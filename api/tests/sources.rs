//! Stage 5 of the web client: the briefing's sources through the door —
//! scope, every source's row from a stub of its provider, the Google token
//! refresh, the 5-minute cache, the OAuth callback, and no token ever in a
//! response.

mod common;

use axum::http::{Method, StatusCode};
use common::{assert_envelope, call, call_raw, harness, harness_with, Options};
use opus_api::auth::keys::Scope;

fn src_calls(h: &common::Harness, path: &str) -> usize {
    h.seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|(m, p, _)| m == "SRC" && p == path)
        .count()
}

#[tokio::test]
async fn sources_need_the_scope_and_exist_only_when_configured() {
    let h = harness().await;
    let key = h.key("all", &[Scope::SourcesRead]);
    let (status, _, _) = call(&h, Method::GET, "/v1/briefing", Some(&key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "no sources configured");

    let h = harness_with(Options {
        sources: true,
        ..Default::default()
    })
    .await;
    let other = h.key("ops", &[Scope::OpsRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/briefing", Some(&other), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&json, "forbidden");
}

#[tokio::test]
async fn every_source_becomes_a_row_and_tokens_stay_inside() {
    let h = harness_with(Options {
        sources: true,
        ..Default::default()
    })
    .await;
    let key = h.key("web", &[Scope::SourcesRead]);

    let (status, _, bytes) = call_raw(
        &h,
        Method::GET,
        "/v1/briefing?since=2026-09-20T00:00:00Z",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    for secret in ["g-fresh", "g-refresh", "client-secret", "buffer-key"] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    let row = |id: &str| {
        json["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} row in {json}"))
    };
    let ids: Vec<&str> = json["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["weather", "calendar", "gmail", "youtube", "whoop", "buffer"]
    );

    assert_eq!(row("weather")["headline"], "84°F, mostly clear · 88°/61°");
    assert_eq!(row("gmail")["headline"], "8 unread");
    assert_eq!(row("gmail")["detail"]["recent"][0]["from"], "Acme");
    assert_eq!(row("gmail")["detail"]["new_since"], 1);
    assert_eq!(
        row("calendar")["headline"],
        "1 event today · first: Standup"
    );
    assert_eq!(row("youtube")["headline"], "1680 views this week (+68%)");
    assert_eq!(row("buffer")["headline"], "1 post queued");
    // Never connected: a down row that says what to do, not a failure.
    assert_eq!(row("whoop")["state"], "down");
    assert!(row("whoop")["headline"]
        .as_str()
        .unwrap()
        .contains("opus-api oauth start whoop"));

    // The expired Google token was refreshed once and shared by all three.
    assert_eq!(src_calls(&h, "/google/token"), 1);

    // Cached: a second briefing reaches no provider.
    let before = h.seen.lock().unwrap().requests.len();
    let (status, _, list) = call(&h, Method::GET, "/v1/sources", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        list["sources"][0]["detail"].is_null(),
        "the list has no detail"
    );
    assert_eq!(h.seen.lock().unwrap().requests.len(), before);

    let (status, _, one) = call(&h, Method::GET, "/v1/sources/weather", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["detail"]["high_f"], 88.0);
    let (status, _, _) = call(&h, Method::GET, "/v1/sources/nope", Some(&key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(
        &h,
        Method::GET,
        "/v1/briefing?since=yesterday",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn oauth_callback_takes_a_state_once_and_connects() {
    let h = harness_with(Options {
        sources: true,
        ..Default::default()
    })
    .await;
    let key = h.key("web", &[Scope::SourcesRead]);

    // Unknown state: refused, nothing exchanged.
    let (status, _, json) = call(
        &h,
        Method::GET,
        "/v1/oauth/whoop/callback?code=whoop-code&state=forged",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");
    assert_eq!(src_calls(&h, "/whoop/token"), 0);

    // A state issued for Google doesn't connect WHOOP.
    h.db.insert_oauth_state("for-google", "google").unwrap();
    let (status, _, _) = call_raw(
        &h,
        Method::GET,
        "/v1/oauth/whoop/callback?code=whoop-code&state=for-google",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    h.db.insert_oauth_state("st-1", "whoop").unwrap();
    let (status, headers, body) = call_raw(
        &h,
        Method::GET,
        "/v1/oauth/whoop/callback?code=whoop-code&state=st-1",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(headers["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    assert!(String::from_utf8_lossy(&body).contains("WHOOP connected"));
    assert_eq!(
        h.db.oauth_token("whoop").unwrap().unwrap().refresh_token,
        "w-refresh"
    );

    // Once only.
    let (status, _, _) = call_raw(
        &h,
        Method::GET,
        "/v1/oauth/whoop/callback?code=whoop-code&state=st-1",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // And WHOOP now reads with the new token.
    let (_, _, whoop) = call(&h, Method::GET, "/v1/sources/whoop", Some(&key), None).await;
    assert_eq!(
        whoop["headline"], "recovery 72% · sleep 91% · strain 9.8",
        "{whoop}"
    );
    let used = h.seen.lock().unwrap().requests.iter().any(|(m, p, b)| {
        m == "SRC"
            && p == "/developer/v2/recovery"
            && b.as_ref().and_then(|v| v.as_str()) == Some("Bearer w-access")
    });
    assert!(used);

    // Refused consent.
    let (status, _, _) = call(
        &h,
        Method::GET,
        "/v1/oauth/google/callback?error=access_denied",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
