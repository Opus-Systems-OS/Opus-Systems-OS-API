//! Cloudflare (`GET /client/v4/zones`, then each zone's DNS records).

use super::{get_json, plural, Report, State};
use crate::error::Result;
use serde_json::{json, Value};

pub async fn check(http: &reqwest::Client, base: &str, token: &str) -> Result<Report> {
    let zones_body = get_json(
        http,
        &format!("{base}/client/v4/zones"),
        token,
        "Cloudflare",
    )
    .await?;
    let mut zones = Vec::new();
    for z in zones_body
        .get("result")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let id = z.get("id").and_then(Value::as_str).unwrap_or("");
        let name = z.get("name").and_then(Value::as_str).unwrap_or("");
        let records_body = get_json(
            http,
            &format!("{base}/client/v4/zones/{id}/dns_records?per_page=100"),
            token,
            "Cloudflare",
        )
        .await?;
        let records: Vec<Value> = records_body
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|r| {
                json!({
                    "type": r.get("type").and_then(Value::as_str).unwrap_or(""),
                    "name": r.get("name").and_then(Value::as_str).unwrap_or(""),
                    "content": r.get("content").and_then(Value::as_str).unwrap_or(""),
                    "proxied": r.get("proxied").and_then(Value::as_bool).unwrap_or(false),
                    "ttl": r.get("ttl").and_then(Value::as_i64).unwrap_or(0),
                })
            })
            .collect();
        zones.push(json!({
            "name": name,
            "status": z.get("status").and_then(Value::as_str).unwrap_or(""),
            "paused": z.get("paused").and_then(Value::as_bool).unwrap_or(false),
            "records": records,
        }));
    }
    let active = zones
        .iter()
        .filter(|z| z["status"] == "active" && z["paused"] == false)
        .count();
    let state = if zones.is_empty() {
        State::Unknown
    } else if active == zones.len() {
        State::Ok
    } else {
        State::Warn
    };
    let headline = zones
        .iter()
        .map(|z| {
            format!(
                "{} {} · {}",
                z["name"].as_str().unwrap_or(""),
                z["status"].as_str().unwrap_or(""),
                plural(
                    z["records"].as_array().map(Vec::len).unwrap_or(0),
                    "record",
                    "records"
                )
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Report {
        state,
        headline: if zones.is_empty() {
            "no zones".into()
        } else {
            headline
        },
        detail: json!({ "zones": zones }),
    })
}
