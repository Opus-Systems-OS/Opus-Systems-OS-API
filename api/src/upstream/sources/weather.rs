//! Open-Meteo forecast for one place (Calabasas by default): no key. Current
//! temperature, conditions and wind; today's high, low and chance of rain.
//! Fahrenheit and mph, the local day in the place's own time zone.

use super::{source_error, Sources};
use crate::error::Result;
use crate::upstream::ops::{read_json, Report, State};
use serde_json::{json, Value};

pub async fn check(s: &Sources) -> Result<Report> {
    let c = &s.cfg;
    let res = s
        .http
        .get(format!("{}/v1/forecast", s.urls.open_meteo))
        .query(&[
            ("latitude", c.latitude.to_string()),
            ("longitude", c.longitude.to_string()),
            (
                "current",
                "temperature_2m,weather_code,wind_speed_10m".into(),
            ),
            (
                "daily",
                "temperature_2m_max,temperature_2m_min,precipitation_probability_max".into(),
            ),
            ("temperature_unit", "fahrenheit".into()),
            ("wind_speed_unit", "mph".into()),
            ("timezone", "America/Los_Angeles".into()),
            ("forecast_days", "1".into()),
        ])
        .send()
        .await?;
    let body = read_json(res, "Open-Meteo").await?;
    report(&body, &c.place)
}

/// The forecast body → the row. Pure, for tests.
pub fn report(body: &Value, place: &str) -> Result<Report> {
    let num = |p: &str| body.pointer(p).and_then(Value::as_f64);
    let temp = num("/current/temperature_2m")
        .ok_or_else(|| source_error("Open-Meteo", "no current temperature"))?;
    let code = body
        .pointer("/current/weather_code")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    let high = num("/daily/temperature_2m_max/0");
    let low = num("/daily/temperature_2m_min/0");
    let rain = num("/daily/precipitation_probability_max/0");
    let wind = num("/current/wind_speed_10m");
    let conditions = describe(code);
    let mut headline = format!("{}°F, {conditions}", temp.round());
    if let (Some(h), Some(l)) = (high, low) {
        headline.push_str(&format!(" · {}°/{}°", h.round(), l.round()));
    }
    Ok(Report {
        state: State::Ok,
        headline,
        detail: json!({
            "place": place,
            "temperature_f": temp.round(),
            "conditions": conditions,
            "weather_code": code,
            "high_f": high.map(f64::round),
            "low_f": low.map(f64::round),
            "rain_chance_pct": rain.map(f64::round),
            "wind_mph": wind.map(f64::round),
        }),
    })
}

/// WMO weather interpretation codes, as a spoken phrase.
pub fn describe(code: i64) -> &'static str {
    match code {
        0 => "clear",
        1 => "mostly clear",
        2 => "partly cloudy",
        3 => "overcast",
        45 | 48 => "foggy",
        51 | 53 | 55 | 56 | 57 => "drizzle",
        61 | 80 => "light rain",
        63 | 81 => "rain",
        65 | 82 => "heavy rain",
        66 | 67 => "freezing rain",
        71 | 73 | 75 | 77 | 85 | 86 => "snow",
        95 => "thunderstorms",
        96 | 99 => "thunderstorms with hail",
        _ => "unsettled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forecast_becomes_a_row() {
        let body = json!({
            "current": {"time": "2026-09-28T14:30", "temperature_2m": 83.6, "weather_code": 1, "wind_speed_10m": 6.2},
            "daily": {"time": ["2026-09-28"], "temperature_2m_max": [88.2], "temperature_2m_min": [61.4], "precipitation_probability_max": [3]}
        });
        let r = report(&body, "Calabasas").unwrap();
        assert_eq!(r.headline, "84°F, mostly clear · 88°/61°");
        assert_eq!(r.detail["high_f"], 88.0);
        assert_eq!(r.detail["rain_chance_pct"], 3.0);
        assert_eq!(r.detail["place"], "Calabasas");
        assert!(report(&json!({}), "x").is_err());
    }
}
