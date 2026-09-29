//! Briefing sources: the day as Jarvis tells it — weather, mail, calendar,
//! YouTube, WHOOP, the Buffer queue. Same shape as `/v1/ops`: each source
//! turns its credential into a headline, a state and a detail document; a
//! source that can't be reached is a `down` row with the reason, never a
//! failed briefing; nothing hands out a token.
//!
//! Each source is fetched at most once per `TTL` however many clients ask.
//! Google and WHOOP use OAuth refresh tokens kept in the API's database
//! (`oauth.rs`); consent starts on the host with `opus-api oauth start`.

pub mod buffer;
pub mod google;
pub mod oauth;
pub mod weather;
pub mod whoop;

use crate::config::SourcesConfig;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::upstream::ops::{reason, Report, State, Status};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long one fetch of a source is reused.
pub const TTL: Duration = Duration::from_secs(300);

/// Every source, in briefing order: `(id, name)`.
pub const SOURCES: [(&str, &str); 6] = [
    ("weather", "Weather"),
    ("calendar", "Calendar"),
    ("gmail", "Gmail"),
    ("youtube", "YouTube"),
    ("whoop", "WHOOP"),
    ("buffer", "Buffer"),
];

/// Overridable base URLs so tests can point every source at a stub.
#[derive(Debug, Clone)]
pub struct BaseUrls {
    pub open_meteo: String,
    pub gmail: String,
    pub calendar: String,
    pub youtube_analytics: String,
    pub youtube_data: String,
    pub google_token: String,
    pub whoop: String,
    pub whoop_token: String,
    pub buffer: String,
}

impl Default for BaseUrls {
    fn default() -> Self {
        BaseUrls {
            open_meteo: "https://api.open-meteo.com".into(),
            gmail: "https://gmail.googleapis.com".into(),
            calendar: "https://www.googleapis.com".into(),
            youtube_analytics: "https://youtubeanalytics.googleapis.com".into(),
            youtube_data: "https://www.googleapis.com".into(),
            google_token: "https://oauth2.googleapis.com/token".into(),
            whoop: "https://api.prod.whoop.com".into(),
            whoop_token: "https://api.prod.whoop.com/oauth/oauth2/token".into(),
            buffer: "https://api.buffer.com".into(),
        }
    }
}

#[derive(Clone)]
pub struct Sources {
    pub(crate) http: reqwest::Client,
    pub(crate) cfg: SourcesConfig,
    pub(crate) urls: BaseUrls,
    pub(crate) db: Db,
    cache: Arc<Mutex<HashMap<&'static str, (Instant, Status)>>>,
    /// One token refresh at a time per provider: WHOOP rotates its refresh
    /// token on use, so two concurrent refreshes would lose one.
    pub(crate) refresh_lock: Arc<Mutex<()>>,
}

impl Sources {
    pub fn new(cfg: SourcesConfig, db: Db) -> Result<Self> {
        Self::with_base_urls(cfg, db, BaseUrls::default())
    }

    pub fn with_base_urls(cfg: SourcesConfig, db: Db, urls: BaseUrls) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("opus-api/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Sources {
            http,
            cfg,
            urls,
            db,
            cache: Arc::new(Mutex::new(HashMap::new())),
            refresh_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Sources this API can serve: weather always; the rest once their
    /// client credentials are in the environment (whether consent has been
    /// given yet shows as the row's state, not as its absence).
    pub fn configured(&self) -> Vec<&'static str> {
        SOURCES
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| self.is_configured(id))
            .collect()
    }

    fn is_configured(&self, id: &str) -> bool {
        match id {
            "weather" => true,
            "gmail" | "calendar" | "youtube" => self.cfg.google.is_some(),
            "whoop" => self.cfg.whoop.is_some(),
            "buffer" => self.cfg.buffer_api_key.is_some(),
            _ => false,
        }
    }

    /// Every configured source, fetched concurrently, with detail (the
    /// briefing needs it).
    pub async fn all(&self) -> Vec<Status> {
        let ids = self.configured();
        futures_util::future::join_all(ids.iter().map(|id| self.status(id)))
            .await
            .into_iter()
            .flatten()
            .collect()
    }

    /// One source with its detail. `Err(NotFound)` for an unknown or
    /// unconfigured id.
    pub async fn status(&self, id: &str) -> Result<Status> {
        let (id, name) = SOURCES
            .iter()
            .copied()
            .find(|(sid, _)| *sid == id)
            .filter(|(sid, _)| self.is_configured(sid))
            .ok_or(Error::NotFound)?;
        if let Some((at, status)) = self.cache.lock().await.get(id) {
            if at.elapsed() < TTL {
                return Ok(status.clone());
            }
        }
        let report = match self.fetch(id).await {
            Ok(r) => r,
            Err(e) => {
                let why = reason(&e);
                Report {
                    state: State::Down,
                    headline: why.clone(),
                    detail: json!({ "error": why }),
                }
            }
        };
        let status = Status {
            id: id.to_owned(),
            name: name.to_owned(),
            state: report.state,
            headline: report.headline,
            checked_at: crate::db::now(),
            detail: Some(report.detail),
        };
        self.cache
            .lock()
            .await
            .insert(id, (Instant::now(), status.clone()));
        Ok(status)
    }

    async fn fetch(&self, id: &str) -> Result<Report> {
        match id {
            "weather" => weather::check(self).await,
            "gmail" => google::gmail(self).await,
            "calendar" => google::calendar(self).await,
            "youtube" => google::youtube(self).await,
            "whoop" => whoop::check(self).await,
            "buffer" => buffer::check(self).await,
            _ => Err(Error::NotFound),
        }
    }

    /// Drop a source's cached row (after new consent, say).
    pub async fn forget(&self, ids: &[&'static str]) {
        let mut cache = self.cache.lock().await;
        for id in ids {
            cache.remove(id);
        }
    }
}

/// An error a source reports about itself, for its row.
pub(crate) fn source_error(source: &str, message: impl Into<String>) -> Error {
    Error::Upstream {
        status: 502,
        kind: "sources".into(),
        message: format!("{source}: {}", message.into()),
        retry_after: None,
    }
}

// ---- time in Calabasas ---------------------------------------------------

/// US Pacific's UTC offset at `utc`, in hours: −7 from the second Sunday of
/// March (02:00 local) to the first Sunday of November (02:00 local), −8
/// otherwise. The whole tz database for one fixed zone would be a
/// dependency for two dates a year.
pub fn pacific_offset(utc: time::OffsetDateTime) -> i8 {
    use time::{Date, Month};
    let year = utc.year();
    let nth_sunday = |month: Month, n: u8| -> Date {
        let first = Date::from_calendar_date(year, month, 1).expect("valid date");
        let to_sunday = (7 - first.weekday().number_days_from_sunday()) % 7;
        first + time::Duration::days(i64::from(to_sunday) + 7 * i64::from(n - 1))
    };
    // 02:00 PST = 10:00 UTC; 02:00 PDT = 09:00 UTC.
    let start = nth_sunday(Month::March, 2)
        .with_hms(10, 0, 0)
        .unwrap()
        .assume_utc();
    let end = nth_sunday(Month::November, 1)
        .with_hms(9, 0, 0)
        .unwrap()
        .assume_utc();
    if utc >= start && utc < end {
        -7
    } else {
        -8
    }
}

/// `utc` as Pacific local time.
pub fn pacific(utc: time::OffsetDateTime) -> time::OffsetDateTime {
    let off = time::UtcOffset::from_hms(pacific_offset(utc), 0, 0).expect("valid offset");
    utc.to_offset(off)
}

/// Start and end of the Pacific day containing `utc`, as RFC 3339 strings
/// with their offset (end = the next midnight).
pub fn pacific_day(utc: time::OffsetDateTime) -> (String, String) {
    let date = pacific(utc).date();
    let fmt = &time::format_description::well_known::Rfc3339;
    (
        pacific_midnight(date).format(fmt).unwrap_or_default(),
        pacific_midnight(date.next_day().expect("in range"))
            .format(fmt)
            .unwrap_or_default(),
    )
}

/// Midnight starting `date` in Calabasas, with that instant's own offset
/// (DST switches at 02:00, so midnight is never ambiguous).
fn pacific_midnight(date: time::Date) -> time::OffsetDateTime {
    let at = |h: i8| {
        time::PrimitiveDateTime::new(date, time::Time::MIDNIGHT)
            .assume_offset(time::UtcOffset::from_hms(h, 0, 0).expect("valid offset"))
    };
    let pdt = at(-7);
    if pacific_offset(pdt) == -7 {
        pdt
    } else {
        at(-8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn pacific_offset_follows_us_dst() {
        // 2026: DST from 8 March 10:00 UTC to 1 November 09:00 UTC.
        assert_eq!(pacific_offset(datetime!(2026-03-08 09:59 UTC)), -8);
        assert_eq!(pacific_offset(datetime!(2026-03-08 10:00 UTC)), -7);
        assert_eq!(pacific_offset(datetime!(2026-09-28 20:00 UTC)), -7);
        assert_eq!(pacific_offset(datetime!(2026-11-01 08:59 UTC)), -7);
        assert_eq!(pacific_offset(datetime!(2026-11-01 09:00 UTC)), -8);
        assert_eq!(pacific_offset(datetime!(2027-01-15 12:00 UTC)), -8);
    }

    #[test]
    fn a_pacific_day_is_local_midnight_to_midnight() {
        // 02:30 UTC on the 29th is still the evening of the 28th in LA.
        let (start, end) = pacific_day(datetime!(2026-09-29 02:30 UTC));
        assert_eq!(start, "2026-09-28T00:00:00-07:00");
        assert_eq!(end, "2026-09-29T00:00:00-07:00");
        // The day DST ends is 25 hours long.
        let (start, end) = pacific_day(datetime!(2026-11-01 12:00 UTC));
        assert_eq!(start, "2026-11-01T00:00:00-07:00");
        assert_eq!(end, "2026-11-02T00:00:00-08:00");
    }
}
