//! WHOOP (developer API v2): the latest recovery, sleep and cycle. Each
//! collection is newest first, so `limit=1` is "last night" / "today".

use super::oauth::{access_token, Provider};
use super::Sources;
use crate::error::Result;
use crate::upstream::ops::{get_json, Report, State};
use serde_json::{json, Value};

pub async fn check(s: &Sources) -> Result<Report> {
    let token = access_token(s, Provider::Whoop).await?;
    let base = format!("{}/developer/v2", s.urls.whoop);
    let (recovery_url, sleep_url, cycle_url) = (
        format!("{base}/recovery?limit=1"),
        format!("{base}/activity/sleep?limit=1"),
        format!("{base}/cycle?limit=1"),
    );
    let (recovery, sleep, cycle) = tokio::join!(
        get_json(&s.http, &recovery_url, &token, "WHOOP"),
        get_json(&s.http, &sleep_url, &token, "WHOOP"),
        get_json(&s.http, &cycle_url, &token, "WHOOP"),
    );
    Ok(report(&recovery?, &sleep?, &cycle?))
}

/// A score field of the newest record, whether nested under `score` or not.
fn score(v: &Value, field: &str) -> Option<f64> {
    let rec = v.pointer("/records/0")?;
    rec.pointer(&format!("/score/{field}"))
        .or_else(|| rec.get(field))
        .and_then(Value::as_f64)
}

pub fn report(recovery: &Value, sleep: &Value, cycle: &Value) -> Report {
    let rec = score(recovery, "recovery_score").map(f64::round);
    let rhr = score(recovery, "resting_heart_rate").map(f64::round);
    let hrv = score(recovery, "hrv_rmssd_milli").map(f64::round);
    let sleep_pct = score(sleep, "sleep_performance_percentage").map(f64::round);
    let strain = score(cycle, "strain").map(|s| (s * 10.0).round() / 10.0);
    // WHOOP's own bands: green ≥ 67, yellow 34–66, red ≤ 33.
    let (state, band) = match rec {
        Some(r) if r >= 67.0 => (State::Ok, "green"),
        Some(r) if r >= 34.0 => (State::Warn, "yellow"),
        Some(_) => (State::Down, "red"),
        None => (State::Unknown, "not scored"),
    };
    let mut parts = vec![match rec {
        Some(r) => format!("recovery {r}%"),
        None => "recovery not scored yet".into(),
    }];
    if let Some(p) = sleep_pct {
        parts.push(format!("sleep {p}%"));
    }
    if let Some(s) = strain {
        parts.push(format!("strain {s}"));
    }
    Report {
        // A red recovery is news, not an outage: keep the row "warn" at worst.
        state: if state == State::Down {
            State::Warn
        } else {
            state
        },
        headline: parts.join(" · "),
        detail: json!({
            "recovery_pct": rec,
            "band": band,
            "resting_hr": rhr,
            "hrv_ms": hrv,
            "sleep_performance_pct": sleep_pct,
            "strain": strain,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_records_become_a_row() {
        let r = report(
            &json!({"records": [{"score_state": "SCORED", "score": {"recovery_score": 41.0, "resting_heart_rate": 58.0, "hrv_rmssd_milli": 44.7}}]}),
            &json!({"records": [{"score": {"sleep_performance_percentage": 78.0}}]}),
            &json!({"records": [{"score": {"strain": 11.26}}]}),
        );
        assert_eq!(r.headline, "recovery 41% · sleep 78% · strain 11.3");
        assert_eq!(r.detail["band"], "yellow");
        assert_eq!(r.state, State::Warn);
        let empty = report(&json!({"records": []}), &json!({}), &json!({}));
        assert_eq!(empty.headline, "recovery not scored yet");
    }
}
