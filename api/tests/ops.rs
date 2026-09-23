//! `/v1/ops*`: the stack's services, read-only, behind `ops:read`.

mod common;

use axum::http::{Method, StatusCode};
use common::{assert_envelope, call, harness, harness_with, Options};
use opus_api::auth::keys::Scope;
use serde_json::json;

fn ops_harness() -> Options {
    Options {
        ops: true,
        ..Options::default()
    }
}

#[tokio::test]
async fn hub_lists_every_configured_service_with_a_headline() {
    let h = harness_with(ops_harness()).await;
    let key = h.key("quest-3", &[Scope::OpsRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/ops", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let services = json["services"].as_array().unwrap();
    let ids: Vec<&str> = services.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "github",
            "uptimerobot",
            "droplet",
            "docker",
            "tailscale",
            "cloudflare"
        ],
        "hub order"
    );
    for s in services {
        assert!(s.get("detail").is_none(), "no detail on the hub: {s}");
        assert!(!s["checked_at"].as_str().unwrap().is_empty());
    }
    let by_id = |id: &str| services.iter().find(|s| s["id"] == id).unwrap().clone();

    let gh = by_id("github");
    assert_eq!(gh["state"], "warn", "one repo's CI failed: {gh}");
    assert_eq!(
        gh["headline"],
        "1 open PR · CI failing: Iron-Fleet · 1 notification"
    );

    let ur = by_id("uptimerobot");
    assert_eq!(ur["state"], "ok");
    assert_eq!(ur["headline"], "2/2 up");

    let dr = by_id("droplet");
    assert_eq!(dr["state"], "ok");
    assert_eq!(
        dr["headline"],
        "opustower nyc1 active · load 0.12 · mem 41%"
    );

    let dk = by_id("docker");
    assert_eq!(dk["state"], "warn", "one exited container");
    assert_eq!(dk["headline"], "2/3 running");

    let ts = by_id("tailscale");
    assert_eq!(ts["state"], "warn", "the rig is off");
    assert_eq!(ts["headline"], "opustower online (1/2)");

    let cf = by_id("cloudflare");
    assert_eq!(cf["state"], "ok");
    assert_eq!(cf["headline"], "opustower.dev active · 3 records");

    // Every upstream call carried the configured bearer (UptimeRobot takes
    // the key in the form body instead — its stub rejects a wrong one).
    let seen = h.seen.lock().unwrap();
    let ops_calls: Vec<&(String, String, Option<serde_json::Value>)> = seen
        .requests
        .iter()
        .filter(|r| r.0.starts_with("ops "))
        .collect();
    assert!(!ops_calls.is_empty());
    for (_, path, _) in &ops_calls {
        if !path.starts_with("/v2/getMonitors") {
            assert!(path.ends_with("bearer_ok=true"), "{path}");
        }
    }
}

#[tokio::test]
async fn service_detail_is_the_document_and_is_cached() {
    let h = harness_with(ops_harness()).await;
    let key = h.key("quest-3", &[Scope::OpsRead]);
    let (status, _, gh) = call(&h, Method::GET, "/v1/ops/github", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{gh}");
    assert_eq!(gh["name"], "GitHub");
    let repos = gh["detail"]["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    assert_eq!(repos[0]["open_prs"][0]["number"], 36);
    assert_eq!(repos[0]["last_run"]["conclusion"], "failure");
    assert_eq!(repos[1]["open_prs"].as_array().unwrap().len(), 0);
    assert_eq!(
        gh["detail"]["notifications"][0]["reason"],
        "review_requested"
    );

    let (_, _, dk) = call(&h, Method::GET, "/v1/ops/docker", Some(&key), None).await;
    let containers = dk["detail"]["containers"].as_array().unwrap();
    assert_eq!(containers[0]["name"], "droplet-api-1");
    assert_eq!(containers[1]["ports"][0], "443->443/tcp");
    assert_eq!(containers[2]["state"], "exited");

    let (_, _, cf) = call(&h, Method::GET, "/v1/ops/cloudflare", Some(&key), None).await;
    assert_eq!(cf["detail"]["zones"][0]["records"][2]["type"], "AAAA");
    let (_, _, ts) = call(&h, Method::GET, "/v1/ops/tailscale", Some(&key), None).await;
    assert_eq!(ts["detail"]["devices"][0]["online"], false);
    assert_eq!(ts["detail"]["devices"][1]["online"], true);
    let (_, _, dr) = call(&h, Method::GET, "/v1/ops/droplet", Some(&key), None).await;
    assert_eq!(dr["detail"]["droplets"][0]["ipv4"][0], "198.199.66.109");
    let (_, _, ur) = call(&h, Method::GET, "/v1/ops/uptimerobot", Some(&key), None).await;
    assert_eq!(ur["detail"]["monitors"][0]["uptime_7d"], "99.987");
    assert_eq!(ur["detail"]["monitors"][0]["response_ms"], 212);

    // Within the TTL, asking again does not poll GitHub again.
    let before = h
        .seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.1.starts_with("/orgs/"))
        .count();
    let (_, _, again) = call(&h, Method::GET, "/v1/ops/github", Some(&key), None).await;
    assert_eq!(again["checked_at"], gh["checked_at"]);
    let after = h
        .seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.1.starts_with("/orgs/"))
        .count();
    assert_eq!(before, after, "served from cache");
}

#[tokio::test]
async fn a_service_that_rejects_the_token_is_a_down_row_not_a_failed_hub() {
    let h = harness_with(Options {
        ops: true,
        github_rejects: true,
        ..Options::default()
    })
    .await;
    let key = h.key("quest-3", &[Scope::OpsRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/ops", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    let gh = json["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "github")
        .unwrap();
    assert_eq!(gh["state"], "down");
    assert_eq!(gh["headline"], "GitHub: 401 Bad credentials");
    let ok = json["services"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["state"] == "ok")
        .count();
    assert!(ok >= 2, "the others still report: {json}");
    let (status, _, detail) = call(&h, Method::GET, "/v1/ops/github", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["detail"]["error"], "GitHub: 401 Bad credentials");
}

#[tokio::test]
async fn unknown_service_is_404_and_scope_is_enforced() {
    let h = harness_with(ops_harness()).await;
    let key = h.key("quest-3", &[Scope::OpsRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/ops/railway", Some(&key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");

    let no_scope = h.key("viewer", &[Scope::FleetRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/ops", Some(&no_scope), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&json, "forbidden");
}

#[tokio::test]
async fn ops_routes_absent_when_unconfigured() {
    let h = harness().await;
    let key = h.key("quest-3", &[Scope::OpsRead]);
    let (status, _, json) = call(&h, Method::GET, "/v1/ops", Some(&key), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");
    let (_, _, spec) = call(&h, Method::GET, "/v1/openapi.json", None, None).await;
    assert!(spec["paths"].get("/ops").is_none());
}

#[tokio::test]
async fn session_client_label_is_forwarded_and_validated() {
    let h = harness().await;
    let key = h.key("quest-3", &[Scope::SessionsWrite]);
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug":"jarvis","task":"hello","client":"quest"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let forwarded = h
        .seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .find(|r| r.0 == "POST" && r.1 == "/sessions")
        .and_then(|r| r.2.clone())
        .unwrap();
    assert_eq!(forwarded["client"], "quest");

    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug":"jarvis","task":"hello","client":"Quest 3!"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&json, "invalid_request");
}

#[tokio::test]
async fn session_model_is_forwarded_and_shape_checked() {
    let h = harness().await;
    let key = h.key("web", &[Scope::SessionsWrite]);
    let (status, _, json) = call(
        &h,
        Method::POST,
        "/v1/sessions",
        Some(&key),
        Some(json!({"agent_slug":"jarvis","task":"hello","model":"claude-sonnet-5"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let forwarded = h
        .seen
        .lock()
        .unwrap()
        .requests
        .iter()
        .find(|r| r.0 == "POST" && r.1 == "/sessions")
        .and_then(|r| r.2.clone())
        .unwrap();
    assert_eq!(forwarded["model"], "claude-sonnet-5");

    for bad in ["", "Claude Sonnet", "claude-sonnet-5; drop"] {
        let (status, _, json) = call(
            &h,
            Method::POST,
            "/v1/sessions",
            Some(&key),
            Some(json!({"agent_slug":"jarvis","task":"hello","model": bad})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_envelope(&json, "invalid_request");
    }
}
