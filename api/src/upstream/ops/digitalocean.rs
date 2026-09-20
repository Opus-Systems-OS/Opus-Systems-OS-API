//! DigitalOcean (`GET /v2/droplets`, plus the monitoring agent's load and
//! memory gauges when the droplet has the agent). Metrics are best-effort:
//! a droplet without the agent still reports.

use super::{get_json, Report, State};
use crate::error::Result;
use serde_json::{json, Value};

pub async fn check(http: &reqwest::Client, base: &str, token: &str) -> Result<Report> {
    let body = get_json(
        http,
        &format!("{base}/v2/droplets?per_page=50"),
        token,
        "DigitalOcean",
    )
    .await?;
    let mut droplets = Vec::new();
    for d in body
        .get("droplets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let id = d.get("id").and_then(Value::as_i64).unwrap_or(0);
        let ipv4 = d
            .pointer("/networks/v4")
            .and_then(Value::as_array)
            .map(|nets| {
                nets.iter()
                    .filter(|n| n.get("type").and_then(Value::as_str) == Some("public"))
                    .filter_map(|n| n.get("ip_address").and_then(Value::as_str))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let load = gauge(http, base, token, "load_1", id).await;
        let mem_total = gauge(http, base, token, "memory_total", id).await;
        let mem_free = gauge(http, base, token, "memory_available", id).await;
        let mem_pct = match (mem_total, mem_free) {
            (Some(t), Some(f)) if t > 0.0 => Some(((t - f) / t * 100.0).round()),
            _ => None,
        };
        droplets.push(json!({
            "name": d.get("name").and_then(Value::as_str).unwrap_or(""),
            "status": d.get("status").and_then(Value::as_str).unwrap_or(""),
            "region": d.pointer("/region/slug").and_then(Value::as_str).unwrap_or(""),
            "ipv4": ipv4,
            "vcpus": d.get("vcpus").and_then(Value::as_i64).unwrap_or(0),
            "memory_mb": d.get("memory").and_then(Value::as_i64).unwrap_or(0),
            "disk_gb": d.get("disk").and_then(Value::as_i64).unwrap_or(0),
            "created_at": d.get("created_at").and_then(Value::as_str).unwrap_or(""),
            "load_1": load,
            "memory_used_pct": mem_pct,
        }));
    }
    let active = droplets.iter().filter(|d| d["status"] == "active").count();
    let state = if droplets.is_empty() {
        State::Unknown
    } else if active == droplets.len() {
        State::Ok
    } else {
        State::Down
    };
    let headline = droplets
        .iter()
        .map(|d| {
            let mut s = format!(
                "{} {} {}",
                d["name"].as_str().unwrap_or(""),
                d["region"].as_str().unwrap_or(""),
                d["status"].as_str().unwrap_or("")
            );
            if let Some(l) = d["load_1"].as_f64() {
                s.push_str(&format!(" · load {l:.2}"));
            }
            if let Some(m) = d["memory_used_pct"].as_f64() {
                s.push_str(&format!(" · mem {m:.0}%"));
            }
            s
        })
        .collect::<Vec<_>>()
        .join("; ");
    Ok(Report {
        state,
        headline: if droplets.is_empty() {
            "no droplets".into()
        } else {
            headline
        },
        detail: json!({ "droplets": droplets }),
    })
}

/// The latest value of a droplet gauge, or None when the agent isn't
/// reporting. Prometheus-shaped: `data.result[0].values[last][1]`.
async fn gauge(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    metric: &str,
    host_id: i64,
) -> Option<f64> {
    let end = time::OffsetDateTime::now_utc().unix_timestamp();
    let start = end - 600;
    let url = format!(
        "{base}/v2/monitoring/metrics/droplet/{metric}?host_id={host_id}&start={start}&end={end}"
    );
    let body = get_json(http, &url, token, "DigitalOcean").await.ok()?;
    let values = body.pointer("/data/result/0/values")?.as_array()?;
    values.last()?.get(1)?.as_str()?.parse::<f64>().ok()
}
