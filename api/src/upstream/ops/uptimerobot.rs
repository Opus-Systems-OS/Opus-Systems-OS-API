//! UptimeRobot (`POST /v2/getMonitors`, form-encoded, the key in the body).
//! Status codes: 0 paused, 1 not checked yet, 2 up, 8 seems down, 9 down.

use super::{plural, read_json, Report, State};
use crate::error::Result;
use serde_json::{json, Value};

pub async fn check(http: &reqwest::Client, base: &str, api_key: &str) -> Result<Report> {
    let form = format!(
        "api_key={}&format=json&response_times=1&response_times_limit=1&custom_uptime_ratios=1-7-30",
        api_key.replace('&', "%26").replace('=', "%3D")
    );
    let res = http
        .post(format!("{base}/v2/getMonitors"))
        .header(
            http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form)
        .send()
        .await?;
    let body = read_json(res, "UptimeRobot").await?;
    if body.get("stat").and_then(Value::as_str) != Some("ok") {
        let msg = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("request rejected");
        return Err(crate::error::Error::Upstream {
            status: 400,
            kind: "ops".into(),
            message: format!("UptimeRobot: {msg}"),
            retry_after: None,
        });
    }
    let monitors: Vec<Value> = body
        .get("monitors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let code = m.get("status").and_then(Value::as_i64).unwrap_or(1);
            let ratios: Vec<&str> = m
                .get("custom_uptime_ratio")
                .and_then(Value::as_str)
                .map(|r| r.split('-').collect())
                .unwrap_or_default();
            json!({
                "name": m.get("friendly_name").and_then(Value::as_str).unwrap_or(""),
                "url": m.get("url").and_then(Value::as_str).unwrap_or(""),
                "status": match code { 0 => "paused", 1 => "pending", 2 => "up", 8 => "seems_down", 9 => "down", _ => "unknown" },
                "uptime_24h": ratios.first().copied().unwrap_or(""),
                "uptime_7d": ratios.get(1).copied().unwrap_or(""),
                "uptime_30d": ratios.get(2).copied().unwrap_or(""),
                "response_ms": m.pointer("/response_times/0/value").and_then(Value::as_i64),
            })
        })
        .collect();
    let up = monitors.iter().filter(|m| m["status"] == "up").count();
    let down = monitors
        .iter()
        .filter(|m| m["status"] == "down" || m["status"] == "seems_down")
        .count();
    let state = if monitors.is_empty() {
        State::Unknown
    } else if down > 0 {
        State::Down
    } else if up < monitors.len() {
        State::Warn
    } else {
        State::Ok
    };
    let headline = if down > 0 {
        format!("{up}/{} up · {} down", monitors.len(), down)
    } else {
        format!("{up}/{} up", monitors.len())
    };
    Ok(Report {
        state,
        headline: if monitors.is_empty() {
            plural(0, "monitor", "monitors")
        } else {
            headline
        },
        detail: json!({ "monitors": monitors }),
    })
}
