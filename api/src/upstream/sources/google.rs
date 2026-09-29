//! Google, read-only, under one consent: Gmail (unread in the inbox and the
//! newest of them), Calendar (today's events on the primary calendar) and
//! YouTube (views over the last 7 days against the 7 before, subscribers).

use super::oauth::{access_token, Provider};
use super::{pacific_day, source_error, Sources};
use crate::error::Result;
use crate::upstream::ops::{plural, read_json, Report, State};
use serde_json::{json, Value};

/// How many unread messages the briefing looks at (sender + subject).
const RECENT_UNREAD: usize = 10;

async fn get(
    s: &Sources,
    url: String,
    query: Vec<(&'static str, String)>,
    service: &'static str,
) -> Result<Value> {
    let token = access_token(s, Provider::Google).await?;
    let res = s
        .http
        .get(url)
        .bearer_auth(token)
        .query(&query)
        .send()
        .await?;
    read_json(res, service).await
}

// ---- Gmail -----------------------------------------------------------------

pub async fn gmail(s: &Sources) -> Result<Report> {
    let base = format!("{}/gmail/v1/users/me", s.urls.gmail);
    let inbox = get(s, format!("{base}/labels/INBOX"), vec![], "Gmail").await?;
    let unread = inbox["messagesUnread"].as_u64().unwrap_or(0);
    let list = get(
        s,
        format!("{base}/messages"),
        vec![
            ("q", "is:unread in:inbox".into()),
            ("maxResults", RECENT_UNREAD.to_string()),
        ],
        "Gmail",
    )
    .await?;
    let ids: Vec<String> = list["messages"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let metas = futures_util::future::join_all(ids.iter().map(|id| {
        get(
            s,
            format!("{base}/messages/{id}"),
            vec![
                ("format", "metadata".into()),
                ("metadataHeaders", "From".into()),
                ("metadataHeaders", "Subject".into()),
            ],
            "Gmail",
        )
    }))
    .await;
    let recent: Vec<Value> = metas
        .into_iter()
        .flatten()
        .map(|m| message_summary(&m))
        .collect();
    Ok(gmail_report(unread, recent))
}

/// A message's metadata → `{from, subject, at}` (`at` RFC 3339).
pub fn message_summary(m: &Value) -> Value {
    let header = |name: &str| {
        m.pointer("/payload/headers")
            .and_then(Value::as_array)
            .and_then(|hs| {
                hs.iter().find(|h| {
                    h["name"]
                        .as_str()
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
                })
            })
            .and_then(|h| h["value"].as_str())
            .unwrap_or("")
            .to_owned()
    };
    let at = m["internalDate"]
        .as_str()
        .and_then(|ms| ms.parse::<i128>().ok())
        .and_then(|ms| time::OffsetDateTime::from_unix_timestamp_nanos(ms * 1_000_000).ok())
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        });
    json!({ "from": sender_name(&header("From")), "subject": header("Subject"), "at": at })
}

/// `"Jane Doe <jane@x.com>"` → `"Jane Doe"`; a bare address stays.
pub fn sender_name(from: &str) -> String {
    match from.split_once('<') {
        Some((name, _)) if !name.trim().is_empty() => name.trim().trim_matches('"').to_owned(),
        _ => from.trim().trim_matches(['<', '>']).to_owned(),
    }
}

pub fn gmail_report(unread: u64, recent: Vec<Value>) -> Report {
    Report {
        state: State::Ok,
        headline: if unread == 0 {
            "inbox clear".into()
        } else {
            format!("{unread} unread")
        },
        detail: json!({ "unread": unread, "recent": recent }),
    }
}

// ---- Calendar ----------------------------------------------------------------

pub async fn calendar(s: &Sources) -> Result<Report> {
    let (start, end) = pacific_day(time::OffsetDateTime::now_utc());
    let body = get(
        s,
        format!("{}/calendar/v3/calendars/primary/events", s.urls.calendar),
        vec![
            ("timeMin", start),
            ("timeMax", end),
            ("singleEvents", "true".into()),
            ("orderBy", "startTime".into()),
            ("maxResults", "25".into()),
            ("timeZone", "America/Los_Angeles".into()),
        ],
        "Calendar",
    )
    .await?;
    Ok(calendar_report(&body))
}

pub fn calendar_report(body: &Value) -> Report {
    let events: Vec<Value> = body["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|e| e["status"] != "cancelled")
                .map(|e| {
                    let all_day = e.pointer("/start/dateTime").is_none();
                    json!({
                        "title": e["summary"].as_str().unwrap_or("(untitled)"),
                        "start": e.pointer("/start/dateTime").or_else(|| e.pointer("/start/date")),
                        "end": e.pointer("/end/dateTime").or_else(|| e.pointer("/end/date")),
                        "all_day": all_day,
                        "location": e["location"].as_str(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let headline = match events.iter().find(|e| e["all_day"] == false) {
        Some(first) => format!(
            "{} today · first: {}",
            plural(events.len(), "event", "events"),
            first["title"].as_str().unwrap_or("")
        ),
        None if events.is_empty() => "nothing today".into(),
        None => plural(events.len(), "all-day event", "all-day events"),
    };
    Report {
        state: State::Ok,
        headline,
        detail: json!({ "events": events }),
    }
}

// ---- YouTube -------------------------------------------------------------------

pub async fn youtube(s: &Sources) -> Result<Report> {
    let today = time::OffsetDateTime::now_utc().date();
    let d = |days: i64| (today - time::Duration::days(days)).to_string();
    let views = |start: String, end: String| {
        get(
            s,
            format!("{}/v2/reports", s.urls.youtube_analytics),
            vec![
                ("ids", "channel==MINE".into()),
                ("startDate", start),
                ("endDate", end),
                ("metrics", "views,subscribersGained,subscribersLost".into()),
            ],
            "YouTube Analytics",
        )
    };
    // Analytics lags a couple of days; both windows end on the same lag.
    let (recent, prior, channel) = tokio::join!(
        views(d(8), d(2)),
        views(d(15), d(9)),
        get(
            s,
            format!("{}/youtube/v3/channels", s.urls.youtube_data),
            vec![
                ("part", "statistics,snippet".into()),
                ("mine", "true".into())
            ],
            "YouTube",
        )
    );
    youtube_report(&recent?, &prior?, &channel?)
}

pub fn youtube_report(recent: &Value, prior: &Value, channel: &Value) -> Result<Report> {
    let row = |v: &Value, i: usize| {
        v.pointer(&format!("/rows/0/{i}"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let (views, prev) = (row(recent, 0), row(prior, 0));
    let net_subs = row(recent, 1) - row(recent, 2);
    let ch = channel
        .pointer("/items/0")
        .ok_or_else(|| source_error("YouTube", "no channel on this Google account"))?;
    let subscribers = ch
        .pointer("/statistics/subscriberCount")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok());
    let change_pct = (prev > 0).then(|| ((views - prev) as f64 / prev as f64 * 100.0).round());
    let mut headline = format!("{views} views this week");
    if let Some(p) = change_pct {
        headline.push_str(&format!(" ({}{p}%)", if p >= 0.0 { "+" } else { "" }));
    }
    Ok(Report {
        state: State::Ok,
        headline,
        detail: json!({
            "channel": ch.pointer("/snippet/title"),
            "views_7d": views,
            "views_prior_7d": prev,
            "change_pct": change_pct,
            "subscribers": subscribers,
            "net_subscribers_7d": net_subs,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn senders_are_names() {
        assert_eq!(sender_name("\"Jane Doe\" <jane@x.com>"), "Jane Doe");
        assert_eq!(sender_name("<no-reply@x.com>"), "no-reply@x.com");
        assert_eq!(sender_name("bob@x.com"), "bob@x.com");
    }

    #[test]
    fn message_metadata_is_summarized() {
        let m = json!({"internalDate": "1790000000000", "payload": {"headers": [
            {"name": "Subject", "value": "Invoice 12"}, {"name": "From", "value": "Acme <billing@acme.com>"}]}});
        let s = message_summary(&m);
        assert_eq!(s["from"], "Acme");
        assert_eq!(s["subject"], "Invoice 12");
        assert!(s["at"].as_str().unwrap().starts_with("2026-09-21T"));
    }

    #[test]
    fn gmail_headline() {
        assert_eq!(gmail_report(0, vec![]).headline, "inbox clear");
        assert_eq!(gmail_report(8, vec![]).headline, "8 unread");
    }

    #[test]
    fn calendar_first_timed_event_leads() {
        let body = json!({"items": [
            {"summary": "Holiday", "start": {"date": "2026-09-28"}, "end": {"date": "2026-09-29"}},
            {"summary": "Standup", "start": {"dateTime": "2026-09-28T09:00:00-07:00"}, "end": {"dateTime": "2026-09-28T09:15:00-07:00"}},
            {"summary": "Gone", "status": "cancelled", "start": {"dateTime": "2026-09-28T11:00:00-07:00"}}
        ]});
        let r = calendar_report(&body);
        assert_eq!(r.headline, "2 events today · first: Standup");
        assert_eq!(r.detail["events"][1]["start"], "2026-09-28T09:00:00-07:00");
        assert_eq!(
            calendar_report(&json!({"items": []})).headline,
            "nothing today"
        );
    }

    #[test]
    fn youtube_compares_weeks() {
        let r = youtube_report(
            &json!({"rows": [[1680, 12, 2]]}),
            &json!({"rows": [[1000, 5, 1]]}),
            &json!({"items": [{"snippet": {"title": "Opus"}, "statistics": {"subscriberCount": "2400"}}]}),
        )
        .unwrap();
        assert_eq!(r.headline, "1680 views this week (+68%)");
        assert_eq!(r.detail["net_subscribers_7d"], 10);
        assert_eq!(r.detail["subscribers"], 2400);
        let none = youtube_report(&json!({}), &json!({}), &json!({"items": [{}]})).unwrap();
        assert_eq!(none.detail["change_pct"], Value::Null);
        assert!(youtube_report(&json!({}), &json!({}), &json!({"items": []})).is_err());
    }
}
