//! Tailscale (`GET /api/v2/tailnet/{tailnet}/devices`). A device counts as
//! online when the API says so, or when it was seen in the last five minutes.

use super::{ago, get_json, Report, State};
use crate::error::Result;
use serde_json::{json, Value};
use std::time::Duration;

pub async fn check(http: &reqwest::Client, base: &str, key: &str, tailnet: &str) -> Result<Report> {
    let body = get_json(
        http,
        &format!("{base}/api/v2/tailnet/{tailnet}/devices"),
        key,
        "Tailscale",
    )
    .await?;
    let devices: Vec<Value> = body
        .get("devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|d| {
            let last_seen = d.get("lastSeen").and_then(Value::as_str).unwrap_or("");
            let online = d.get("online").and_then(Value::as_bool).unwrap_or_else(|| {
                ago(last_seen).map(|a| a < Duration::from_secs(300)).unwrap_or(false)
            });
            let name = d
                .get("hostname")
                .and_then(Value::as_str)
                .or_else(|| d.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .split('.')
                .next()
                .unwrap_or("")
                .to_owned();
            json!({
                "name": name,
                "os": d.get("os").and_then(Value::as_str).unwrap_or(""),
                "online": online,
                "last_seen": last_seen,
                "addresses": d.get("addresses").cloned().unwrap_or_else(|| json!([])),
                "update_available": d.get("updateAvailable").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect();
    let online: Vec<&str> = devices
        .iter()
        .filter(|d| d["online"] == true)
        .filter_map(|d| d["name"].as_str())
        .collect();
    let state = if devices.is_empty() {
        State::Unknown
    } else if online.is_empty() {
        State::Down
    } else if online.len() < devices.len() {
        State::Warn
    } else {
        State::Ok
    };
    let headline = if online.is_empty() {
        format!("0/{} online", devices.len())
    } else {
        format!(
            "{} online ({}/{})",
            online.join(", "),
            online.len(),
            devices.len()
        )
    };
    Ok(Report {
        state,
        headline,
        detail: json!({ "devices": devices }),
    })
}
