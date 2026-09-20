//! `/v1/pair*`: a device with no key gets one, approved by a client with
//! `pair:approve`, with the fixed device profile.

mod common;

use axum::http::{Method, StatusCode};
use common::{assert_envelope, call, harness};
use opus_api::auth::keys::Scope;
use serde_json::json;

#[tokio::test]
async fn full_round_trip_mints_a_device_key_and_hands_it_over_once() {
    let h = harness().await;

    // The device starts: no key at all.
    let (status, _, started) = call(&h, Method::POST, "/v1/pair", None, None).await;
    assert_eq!(status, StatusCode::CREATED, "{started}");
    let code = started["code"].as_str().unwrap().to_owned();
    let token = started["token"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 6);
    assert!(code.chars().all(|c| c.is_ascii_digit()));
    assert_eq!(token.len(), 64);
    assert_eq!(started["expires_in"], 600);

    // Not approved yet: 202, nothing handed over.
    let (status, _, _) = call(
        &h,
        Method::GET,
        &format!("/v1/pair/{code}?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // An older key with the device's name exists; approval rotates it out.
    let old = h.key("quest-3", &[Scope::SessionsRead]);
    let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&old), None).await;
    assert_eq!(status, StatusCode::OK);

    // The Mac approves.
    let mac = h.key("mac", &[Scope::PairApprove]);
    let (status, _, approved) = call(
        &h,
        Method::POST,
        &format!("/v1/pair/{code}/approve"),
        Some(&mac),
        Some(json!({"name":"quest-3","wit_token":"wit-abc","speaker":"192.168.1.174:48100"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{approved}");
    assert_eq!(approved["name"], "quest-3");
    let scopes: Vec<&str> = approved["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        scopes,
        [
            "fleet:read",
            "sessions:read",
            "sessions:write",
            "voice",
            "ops:read"
        ],
        "the fixed device profile"
    );
    assert!(
        approved.get("api_key").is_none(),
        "the approver never sees the key"
    );

    let (status, _, _) = call(&h, Method::GET, "/v1/me", Some(&old), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the older quest-3 key was revoked"
    );

    // The device collects — once.
    let (status, _, bundle) = call(
        &h,
        Method::GET,
        &format!("/v1/pair/{code}?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bundle}");
    let api_key = bundle["api_key"].as_str().unwrap();
    assert!(api_key.starts_with("osk_"));
    assert_eq!(bundle["wit_token"], "wit-abc");
    assert_eq!(bundle["speaker"], "192.168.1.174:48100");
    assert_eq!(bundle["key_id"], approved["key_id"]);

    let (status, _, me) = call(&h, Method::GET, "/v1/me", Some(api_key), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["name"], "quest-3");
    assert_eq!(me["scopes"].as_array().unwrap().len(), 5);

    let (status, _, json) = call(
        &h,
        Method::GET,
        &format!("/v1/pair/{code}?token={token}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "gone after the claim: {json}"
    );
    assert_envelope(&json, "not_found");
}

#[tokio::test]
async fn a_code_alone_collects_nothing_and_a_wrong_code_cannot_be_approved() {
    let h = harness().await;
    let (_, _, started) = call(&h, Method::POST, "/v1/pair", None, None).await;
    let code = started["code"].as_str().unwrap().to_owned();
    let mac = h.key("mac", &[Scope::PairApprove]);
    let (status, _, _) = call(
        &h,
        Method::POST,
        &format!("/v1/pair/{code}/approve"),
        Some(&mac),
        Some(json!({"name":"quest-3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Wrong token → as if the code did not exist.
    let (status, _, json) = call(
        &h,
        Method::GET,
        &format!("/v1/pair/{code}?token=0000"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&json, "not_found");
    // Missing token → bad request, not a leak.
    let (status, _, _) = call(&h, Method::GET, &format!("/v1/pair/{code}"), None, None).await;
    assert_ne!(status, StatusCode::OK);

    // Unknown code cannot be approved; an approved code cannot be approved twice.
    let (status, _, _) = call(
        &h,
        Method::POST,
        "/v1/pair/000000/approve",
        Some(&mac),
        Some(json!({"name":"x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = call(
        &h,
        Method::POST,
        &format!("/v1/pair/{code}/approve"),
        Some(&mac),
        Some(json!({"name":"quest-3"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn approving_needs_the_scope_and_the_profile_cannot_be_widened() {
    let h = harness().await;
    let (_, _, started) = call(&h, Method::POST, "/v1/pair", None, None).await;
    let code = started["code"].as_str().unwrap().to_owned();

    let device = h.key("quest-3", &[Scope::SessionsWrite, Scope::OpsRead]);
    let (status, _, json) = call(
        &h,
        Method::POST,
        &format!("/v1/pair/{code}/approve"),
        Some(&device),
        Some(json!({"name":"evil"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&json, "forbidden");

    let (status, _, _) = call(
        &h,
        Method::POST,
        &format!("/v1/pair/{code}/approve"),
        None,
        Some(json!({"name":"evil"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The body has no way to ask for scopes at all.
    let mac = h.key("mac", &[Scope::PairApprove]);
    let (status, _, json) = call(
        &h,
        Method::POST,
        &format!("/v1/pair/{code}/approve"),
        Some(&mac),
        Some(json!({"name":"quest-3","scopes":["keys:admin"]})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test]
async fn starting_is_rate_limited_per_address() {
    let h = harness().await;
    let mut last = StatusCode::OK;
    for _ in 0..12 {
        let (status, _, _) = call(&h, Method::POST, "/v1/pair", None, None).await;
        last = status;
        if status == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
    }
    assert_eq!(
        last,
        StatusCode::TOO_MANY_REQUESTS,
        "an address cannot mint codes without limit"
    );
}
