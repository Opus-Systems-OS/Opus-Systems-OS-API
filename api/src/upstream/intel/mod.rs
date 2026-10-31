//! Intel: open-source news wires and hazard feeds, fetched from a fixed
//! list, normalized into one shape, sorted into categories and placed on
//! the map. The Quest's globe and the web HUD's Intel tab read the same
//! thing. Ported from jarvis-web's `server/src/intel.rs` (the fetching and
//! parsing) and its page's `hud/intel/geo.ts` (the placing).
//!
//! - The feed: every source at once, cached five minutes (one refresh at a
//!   time; a failing source is a `sources[]` row, never a failed feed).
//! - A search: news coverage of a query plus matching Wikipedia entries
//!   (public figures, places, organizations), cached ten minutes per query.
//!
//! Every URL is a constant — a search only fills in the query string of a
//! fixed host — every link handed out is http(s), and text is plain.

pub mod geo;
pub mod parse;

use crate::error::{Error, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

const FEED_TTL: Duration = Duration::from_secs(5 * 60);
const SEARCH_TTL: Duration = Duration::from_secs(10 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_BODY: usize = 6 * 1024 * 1024;
pub(crate) const MAX_SUMMARY: usize = 420;
pub(crate) const PER_FEED: usize = 40;
pub const MAX_QUERY: usize = 80;
const MAX_SEARCHES_CACHED: usize = 64;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Format {
    Rss,
    Usgs,
    Eonet,
    CalFire,
    Nws,
    Inciweb,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub name: &'static str,
    pub url: String,
    pub format: Format,
}

/// Where everything is fetched from. `Default` is the real list; tests
/// point it at a stub.
#[derive(Clone, Debug)]
pub struct IntelUrls {
    pub sources: Vec<Source>,
    /// Google News search RSS.
    pub news_search: String,
    /// The Wikipedia action API.
    pub wikipedia: String,
}

impl Default for IntelUrls {
    fn default() -> Self {
        let s = |name, url: &str, format| Source {
            name,
            url: url.to_owned(),
            format,
        };
        use Format::*;
        IntelUrls {
            sources: vec![
                s("Google News · Top", "https://news.google.com/rss?hl=en-US&gl=US&ceid=US:en", Rss),
                s("Google News · U.S.", "https://news.google.com/rss/headlines/section/topic/NATION?hl=en-US&gl=US&ceid=US:en", Rss),
                s("Google News · World", "https://news.google.com/rss/headlines/section/topic/WORLD?hl=en-US&gl=US&ceid=US:en", Rss),
                s("Google News · Incidents", "https://news.google.com/rss/search?q=wildfire+OR+shooting+OR+explosion+OR+evacuation+OR+police+OR+crash+when:1d&hl=en-US&gl=US&ceid=US:en", Rss),
                s("NPR", "https://feeds.npr.org/1001/rss.xml", Rss),
                s("BBC World", "https://feeds.bbci.co.uk/news/world/rss.xml", Rss),
                s("BBC US & Canada", "https://feeds.bbci.co.uk/news/world/us_and_canada/rss.xml", Rss),
                s("CBS News", "https://www.cbsnews.com/latest/rss/main", Rss),
                s("ABC News", "https://abcnews.com/abcnews/usheadlines", Rss),
                s("Guardian US", "https://www.theguardian.com/us-news/rss", Rss),
                s("Al Jazeera", "https://www.aljazeera.com/xml/rss/all.xml", Rss),
                s("USGS quakes", "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary/2.5_day.geojson", Usgs),
                s("NASA EONET", "https://eonet.gsfc.nasa.gov/api/v3/events?status=open&days=30&limit=200", Eonet),
                s("CAL FIRE", "https://incidents.fire.ca.gov/umbraco/api/IncidentApi/List?inactive=false", CalFire),
                s("InciWeb", "https://inciweb.wildfire.gov/incidents/rss.xml", Inciweb),
                s("NWS alerts", "https://api.weather.gov/alerts/active?status=actual&severity=Severe,Extreme", Nws),
            ],
            news_search: "https://news.google.com/rss/search".into(),
            wikipedia: "https://en.wikipedia.org/w/api.php".into(),
        }
    }
}

/// One event or story, placed and categorized.
#[derive(Serialize, Clone, Debug, Default, utoipa::ToSchema)]
pub struct Item {
    pub id: String,
    /// `news`, `quake`, `fire`, `storm`, `volcano` or `alert`.
    pub kind: &'static str,
    pub title: String,
    pub summary: String,
    pub source: String,
    /// http(s), or empty.
    pub url: String,
    /// RFC 3339, UTC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// Where it is, as a display name ("Bend, OR", "Flood Warning · Iowa").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    /// How the place was found: 4 the feed's own coordinates, 3 a city,
    /// 2 a state or region, 1.5 the capital by institution ("Senate"),
    /// 1 a country, 0.5 a country by demonym. Absent when unplaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place_level: Option<f64>,
    /// A US state's postal code, when that is all the feed said.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 0–1, hazards only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<f64>,
    /// `fire`, `violence`, `conflict`, `unrest`, `disaster`, `weather`,
    /// `politics`, `health`, `economy` or `other`.
    pub category: &'static str,
    /// Which feed it came from.
    pub feed: &'static str,
}

#[derive(Serialize, Debug, utoipa::ToSchema)]
pub struct SourceStatus {
    pub name: &'static str,
    pub ok: bool,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Feed {
    pub generated_at: String,
    /// Newest first; news deduplicated across wires.
    pub items: Vec<Item>,
    pub sources: Vec<SourceStatus>,
}

/// A Wikipedia entry matching a search.
#[derive(Serialize, Debug, utoipa::ToSchema)]
pub struct Entry {
    pub title: String,
    pub description: String,
    pub extract: String,
    pub url: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SearchResult {
    pub query: String,
    /// News coverage, placed where the headline names a place.
    pub articles: Vec<Item>,
    /// Wikipedia entries, search order (several people sharing a name each appear).
    pub entries: Vec<Entry>,
    /// Which half failed, if one did.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// The feeds and their caches (memory only).
#[derive(Clone)]
pub struct Intel {
    http: reqwest::Client,
    urls: Arc<IntelUrls>,
    feed: Arc<tokio::sync::Mutex<Option<(Instant, Bytes)>>>,
    searches: Arc<Mutex<HashMap<String, (Instant, Bytes)>>>,
}

impl Intel {
    pub fn new() -> Result<Self> {
        Self::with_urls(IntelUrls::default())
    }

    pub fn with_urls(urls: IntelUrls) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "opus-api/",
                env!("CARGO_PKG_VERSION"),
                " (+https://api.opustower.dev)"
            ))
            .connect_timeout(Duration::from_secs(5))
            .timeout(FETCH_TIMEOUT)
            // Public feeds do move (http → https, a new path); not far.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        Ok(Intel {
            http,
            urls: Arc::new(urls),
            feed: Default::default(),
            searches: Default::default(),
        })
    }

    /// The whole feed as JSON (`Feed`), from the cache when it is fresh.
    pub async fn feed(&self) -> Bytes {
        // Held across the fetch: one refresh at a time, everyone else waits for it.
        let mut cached = self.feed.lock().await;
        if let Some((at, body)) = cached.as_ref() {
            if at.elapsed() < FEED_TTL {
                return body.clone();
            }
        }
        let http = &self.http;
        let results =
            futures_util::future::join_all(self.urls.sources.iter().map(|s| async move {
                let r = fetch(http, &s.url, accept(s.format))
                    .await
                    .and_then(|b| parse_body(s.name, s.format, &b));
                (s, r)
            }))
            .await;

        let mut items = Vec::new();
        let mut sources = Vec::new();
        for (s, r) in results {
            match r {
                Ok(list) => {
                    sources.push(SourceStatus {
                        name: s.name,
                        ok: true,
                        count: list.len(),
                        error: None,
                    });
                    items.extend(list);
                }
                Err(e) => {
                    tracing::warn!(source = s.name, error = %e, "intel source failed");
                    sources.push(SourceStatus {
                        name: s.name,
                        ok: false,
                        count: 0,
                        error: Some(e),
                    });
                }
            }
        }
        let mut items = parse::dedupe(items);
        items.iter_mut().for_each(enrich);
        let body = to_bytes(&Feed {
            generated_at: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_default(),
            items,
            sources,
        });
        *cached = Some((Instant::now(), body.clone()));
        body
    }

    /// A search as JSON (`SearchResult`). `q` must already be `clean_query`'d.
    pub async fn search(&self, q: &str) -> Result<Bytes> {
        let key = q.to_lowercase();
        {
            let cache = self.searches.lock().expect("search cache");
            if let Some((at, body)) = cache.get(&key) {
                if at.elapsed() < SEARCH_TTL {
                    return Ok(body.clone());
                }
            }
        }
        let bad_url = |e| Error::InvalidRequest(format!("search: {e}"));
        let news_url = reqwest::Url::parse_with_params(
            &self.urls.news_search,
            &[("q", q), ("hl", "en-US"), ("gl", "US"), ("ceid", "US:en")],
        )
        .map_err(bad_url)?;
        let wiki_url = reqwest::Url::parse_with_params(
            &self.urls.wikipedia,
            &[
                ("action", "query"),
                ("generator", "search"),
                ("gsrsearch", q),
                ("gsrlimit", "10"),
                ("prop", "description|extracts"),
                ("exintro", "1"),
                ("explaintext", "1"),
                ("exsentences", "2"),
                ("exlimit", "10"),
                ("format", "json"),
                ("formatversion", "2"),
            ],
        )
        .map_err(bad_url)?;
        let (news, wiki) = tokio::join!(
            fetch(&self.http, news_url.as_str(), accept(Format::Rss)),
            fetch(&self.http, wiki_url.as_str(), "application/json"),
        );
        let mut errors = Vec::new();
        let articles = match news.and_then(|b| parse_body("Google News · Search", Format::Rss, &b))
        {
            Ok(mut a) => {
                a.truncate(30);
                a.iter_mut().for_each(enrich);
                a
            }
            Err(e) => {
                errors.push(format!("news: {e}"));
                Vec::new()
            }
        };
        let entries = match wiki.and_then(|b| parse::parse_wikipedia(&b)) {
            Ok(e) => e,
            Err(e) => {
                errors.push(format!("wikipedia: {e}"));
                Vec::new()
            }
        };
        let body = to_bytes(&SearchResult {
            query: q.to_owned(),
            articles,
            entries,
            errors,
        });
        let mut cache = self.searches.lock().expect("search cache");
        cache.retain(|_, (at, _)| at.elapsed() < SEARCH_TTL);
        if cache.len() < MAX_SEARCHES_CACHED {
            cache.insert(key, (Instant::now(), body.clone()));
        }
        Ok(body)
    }
}

fn to_bytes<T: Serialize>(v: &T) -> Bytes {
    // Plain structs of strings and numbers: serializing cannot fail.
    Bytes::from(serde_json::to_vec(v).expect("intel json"))
}

/// A query fit to send on: whitespace collapsed, 1–80 characters, no
/// control characters.
pub fn clean_query(q: &str) -> Option<String> {
    let q = q.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = q.chars().count();
    if n == 0 || n > MAX_QUERY || q.chars().any(char::is_control) {
        return None;
    }
    Some(q)
}

/// Category always; a place for anything the feed didn't place: the state
/// for a state-only alert (spread a little so one state's alerts don't
/// stack), the gazetteer's best match for news.
pub fn enrich(item: &mut Item) {
    item.category = geo::classify(item.kind, &item.title, &item.summary);
    if item.lat.is_some() && item.lon.is_some() {
        item.place_level = Some(4.0);
        return;
    }
    if let Some(state) = item.region.as_deref().and_then(geo::state_by_code) {
        item.lat = Some(state.lat + geo::jitter(&item.id, 0));
        item.lon = Some(state.lon + geo::jitter(&item.id, 1));
        item.place = Some(match item.place.take() {
            Some(p) => format!("{p} · {}", state.name),
            None => state.name,
        });
        item.place_level = Some(2.0);
        return;
    }
    let found = geo::locate(&item.title).or_else(|| geo::locate(&item.summary));
    if let Some(p) = found {
        item.lat = Some(p.lat);
        item.lon = Some(p.lon);
        item.place = Some(p.name);
        item.place_level = Some(p.level);
    }
}

fn accept(format: Format) -> &'static str {
    match format {
        Format::Rss | Format::Inciweb => "application/rss+xml, application/xml, text/xml",
        Format::Nws => "application/geo+json",
        _ => "application/json",
    }
}

async fn fetch(
    http: &reqwest::Client,
    url: &str,
    accept: &str,
) -> std::result::Result<Vec<u8>, String> {
    let res = http
        .get(url)
        .header(http::header::ACCEPT, accept)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "timed out".to_owned()
            } else {
                "unreachable".to_owned()
            }
        })?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    let mut out = Vec::new();
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "read failed".to_owned())?;
        if out.len() + chunk.len() > MAX_BODY {
            return Err("too large".to_owned());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn parse_body(
    name: &'static str,
    format: Format,
    body: &[u8],
) -> std::result::Result<Vec<Item>, String> {
    let text = std::str::from_utf8(body).map_err(|_| "not UTF-8".to_owned())?;
    match format {
        Format::Rss => parse::parse_rss(name, text),
        Format::Inciweb => parse::parse_inciweb(text),
        Format::Usgs => parse::parse_usgs(text),
        Format::Eonet => parse::parse_eonet(text),
        Format::CalFire => parse::parse_calfire(text),
        Format::Nws => parse::parse_nws(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_bounded() {
        assert_eq!(clean_query("  Jane   Doe "), Some("Jane Doe".into()));
        assert_eq!(clean_query("   "), None);
        assert_eq!(clean_query(&"x".repeat(81)), None);
        assert_eq!(clean_query("a\u{7}b"), None);
    }

    #[test]
    fn news_is_placed_and_categorized() {
        let mut i = Item {
            id: "x".into(),
            kind: "news",
            title: "Wildfire near Malibu forces evacuations".into(),
            ..Default::default()
        };
        enrich(&mut i);
        assert_eq!(i.category, "fire");
        assert_eq!(i.place.as_deref(), Some("Malibu, CA"));
        assert_eq!(i.place_level, Some(3.0));
        assert!(i.lat.is_some() && i.lon.is_some());
    }

    #[test]
    fn a_state_only_alert_sits_near_its_state() {
        let mut i = Item {
            id: "nws-1".into(),
            kind: "alert",
            title: "Flood Warning".into(),
            place: Some("Flood Warning".into()),
            region: Some("IA".into()),
            ..Default::default()
        };
        enrich(&mut i);
        assert_eq!(i.category, "disaster");
        assert_eq!(i.place.as_deref(), Some("Flood Warning · Iowa"));
        assert!((i.lat.unwrap() - 42.1).abs() <= 0.8);
        assert_eq!(i.place_level, Some(2.0));
    }

    #[test]
    fn a_placed_hazard_keeps_its_coordinates() {
        let mut i = Item {
            id: "usgs-1".into(),
            kind: "quake",
            title: "M 3.0 - 5 km N of Ridgecrest, CA".into(),
            lat: Some(35.7),
            lon: Some(-117.6),
            ..Default::default()
        };
        enrich(&mut i);
        assert_eq!((i.lat, i.lon), (Some(35.7), Some(-117.6)));
        assert_eq!(i.place_level, Some(4.0));
        assert_eq!(i.category, "disaster");
    }

    #[test]
    fn an_unplaceable_story_stays_unplaced() {
        let mut i = Item {
            id: "x".into(),
            kind: "news",
            title: "A quiet day".into(),
            ..Default::default()
        };
        enrich(&mut i);
        assert_eq!(i.lat, None);
        assert_eq!(i.place_level, None);
        assert_eq!(i.category, "other");
    }
}
