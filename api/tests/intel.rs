//! `/v1/intel/*`: news and hazards, categorized and placed, behind `intel:read`.

mod common;

use axum::http::{Method, StatusCode};
use common::{assert_envelope, call, harness};
use opus_api::auth::keys::Scope;
use serde_json::Value;

fn item<'a>(feed: &'a Value, title: &str) -> &'a Value {
    feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["title"] == title)
        .unwrap_or_else(|| panic!("no item {title:?} in {feed}"))
}

#[tokio::test]
async fn feed_merges_every_source_and_reports_the_failed_one() {
    let h = harness().await;
    let key = h.key("quest-3", &[Scope::IntelRead]);
    let (status, _, feed) = call(&h, Method::GET, "/v1/intel/feed", Some(&key), None).await;
    assert_eq!(status, StatusCode::OK, "{feed}");

    let sources = feed["sources"].as_array().unwrap();
    assert_eq!(sources.len(), 5);
    let cbs = sources.iter().find(|s| s["name"] == "CBS News").unwrap();
    assert_eq!(cbs["ok"], false);
    assert_eq!(cbs["error"], "HTTP 500");
    assert!(sources.iter().filter(|s| s["ok"] == true).count() == 4);

    // The same story from two wires is one item; newest first.
    let senate: Vec<_> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["title"] == "Senate passes funding bill")
        .collect();
    assert_eq!(senate.len(), 1, "deduplicated");
    let published: Vec<&str> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["published"].as_str())
        .collect();
    let mut sorted = published.clone();
    sorted.sort_by(|a, b| b.cmp(a));
    assert_eq!(published, sorted, "newest first");

    // News is placed by its words and categorized; the outlet is not the place.
    let malibu = item(&feed, "Wildfire near Malibu forces evacuations");
    assert_eq!(malibu["source"], "Los Angeles Times");
    assert_eq!(malibu["category"], "fire");
    assert_eq!(malibu["place"], "Malibu, CA");
    assert_eq!(malibu["place_level"], 3.0);
    assert!(malibu["lat"].is_f64() && malibu["lon"].is_f64());
    assert_eq!(senate[0]["category"], "politics");
    assert_eq!(senate[0]["place"], "Washington, D.C.");

    // Hazards keep the feed's coordinates; a state-only alert sits on its state.
    let quake = item(&feed, "M 4.5 - 5 km N of Ridgecrest, CA");
    assert_eq!(
        (quake["lat"].as_f64(), quake["lon"].as_f64()),
        (Some(35.7), Some(-117.6))
    );
    assert_eq!(quake["place_level"], 4.0);
    assert_eq!(quake["category"], "disaster");
    let flood = item(&feed, "Flood Warning issued");
    assert_eq!(flood["place"], "Flood Warning · Iowa");
    assert_eq!(flood["region"], "IA");
    assert_eq!(flood["category"], "disaster");
    assert_eq!(flood["published"], "2026-10-16T23:00:00Z", "UTC");

    // Unplaceable stays unplaced; an unsafe link is dropped.
    let zoo = item(&feed, "A quiet day at the zoo");
    assert!(zoo.get("lat").is_none());
    assert_eq!(zoo["url"], "");
    assert_eq!(zoo["category"], "other");
}

#[tokio::test]
async fn feed_is_fetched_once_per_five_minutes() {
    let h = harness().await;
    let key = h.key("quest-3", &[Scope::IntelRead]);
    let (a, _, first) = call(&h, Method::GET, "/v1/intel/feed", Some(&key), None).await;
    let (b, _, second) = call(&h, Method::GET, "/v1/intel/feed", Some(&key), None).await;
    assert_eq!((a, b), (StatusCode::OK, StatusCode::OK));
    assert_eq!(first, second);
    let npr = h
        .intel_hits
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.as_str() == "/npr")
        .count();
    assert_eq!(npr, 1, "the second read is the cache");
}

#[tokio::test]
async fn search_returns_coverage_and_entries() {
    let h = harness().await;
    let key = h.key("quest-3", &[Scope::IntelRead]);
    let (status, _, res) = call(
        &h,
        Method::GET,
        "/v1/intel/search?q=%20Jane%20%20Doe%20",
        Some(&key),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{res}");
    assert_eq!(res["query"], "Jane Doe");
    assert_eq!(res["entries"][0]["title"], "Jane Doe");
    assert_eq!(
        res["entries"][0]["url"],
        "https://en.wikipedia.org/?curid=7"
    );
    let a = &res["articles"][0];
    assert_eq!(a["title"], "Jane Doe visits Kyiv");
    assert_eq!(a["source"], "Reuters");
    assert_eq!(a["place"], "Kyiv, Ukraine");
    assert!(res.get("errors").is_none());
}

#[tokio::test]
async fn search_needs_a_bounded_query() {
    let h = harness().await;
    let key = h.key("quest-3", &[Scope::IntelRead]);
    for path in ["/v1/intel/search", "/v1/intel/search?q=%20%20"] {
        let (status, _, json) = call(&h, Method::GET, path, Some(&key), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_envelope(&json, "invalid_request");
    }
    let long = format!("/v1/intel/search?q={}", "x".repeat(81));
    let (status, _, _) = call(&h, Method::GET, &long, Some(&key), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn intel_needs_its_scope() {
    let h = harness().await;
    let key = h.key("mac", &[Scope::FleetRead, Scope::OpsRead]);
    for path in ["/v1/intel/feed", "/v1/intel/search?q=x"] {
        let (status, _, json) = call(&h, Method::GET, path, Some(&key), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_envelope(&json, "forbidden");
    }
    let (status, _, _) = call(&h, Method::GET, "/v1/intel/feed", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        h.intel_hits.lock().unwrap().is_empty(),
        "nothing fetched for a refused key"
    );
}

#[tokio::test]
async fn the_spec_has_both_routes() {
    let h = harness().await;
    let (status, _, spec) = call(&h, Method::GET, "/v1/openapi.json", None, None).await;
    assert_eq!(status, StatusCode::OK);
    for path in ["/intel/feed", "/intel/search"] {
        assert!(spec["paths"][path]["get"].is_object(), "{path} in the spec");
    }
}
