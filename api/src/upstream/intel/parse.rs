//! The feeds' formats, parsed into `Item`s: RSS and Atom wires, USGS
//! GeoJSON, NASA EONET, CAL FIRE, InciWeb, NWS alerts, and Wikipedia
//! search. Ported from jarvis-web's `server/src/intel.rs`. Text is stripped
//! of markup here and links are http(s) only; clients still render it as
//! plain text.

use super::{Entry, Item, MAX_SUMMARY, PER_FEED};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use time::format_description::well_known::{Rfc2822, Rfc3339};
use time::OffsetDateTime;

// ---- text helpers ------------------------------------------------------------

/// Plain text from a feed's HTML fragment: tags dropped, common entities
/// decoded, whitespace collapsed, cut to `MAX_SUMMARY` characters.
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&out);
    clip(
        &decoded.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_SUMMARY,
    )
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &tail[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "rsquo" => Some('’'),
                "lsquo" => Some('‘'),
                "rdquo" => Some('”'),
                "ldquo" => Some('“'),
                n if n.starts_with("#x") => u32::from_str_radix(&n[2..], 16)
                    .ok()
                    .and_then(char::from_u32),
                n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &tail[e + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Only http(s) links reach the page.
fn safe_url(u: &str) -> String {
    let u = u.trim();
    if u.starts_with("https://") || u.starts_with("http://") {
        u.to_owned()
    } else {
        String::new()
    }
}

fn rfc3339_from_rfc2822(s: &str) -> Option<String> {
    let s = s.trim();
    OffsetDateTime::parse(s, &Rfc2822)
        .or_else(|_| OffsetDateTime::parse(s, &Rfc3339))
        .ok()
        .and_then(|t| t.to_offset(time::UtcOffset::UTC).format(&Rfc3339).ok())
}

fn rfc3339_from_millis(ms: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
}

fn short_hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())[..6]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn norm_title(t: &str) -> String {
    t.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Same story from several wires: keep the first. Newest first overall.
pub fn dedupe(items: Vec<Item>) -> Vec<Item> {
    let mut seen = HashSet::new();
    let mut out: Vec<Item> = items
        .into_iter()
        .filter(|i| i.kind != "news" || seen.insert(norm_title(&i.title)))
        .map(|mut i| {
            // Feeds stamp their own offsets (Guam's alerts are +10:00); in
            // UTC the strings sort as times.
            i.published = i.published.as_deref().and_then(rfc3339_from_rfc2822);
            i
        })
        .collect();
    out.sort_by(|a, b| b.published.cmp(&a.published));
    out
}

// ---- RSS / Atom ----------------------------------------------------------------

fn child_text<'a>(node: roxmltree::Node<'a, 'a>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
        .and_then(|c| c.text())
}

pub fn parse_rss(feed: &'static str, xml: &str) -> std::result::Result<Vec<Item>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| "not XML".to_owned())?;
    let google = feed.starts_with("Google News");
    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.is_element()) {
        let tag = node.tag_name().name();
        let (title, link, summary, published) = match tag {
            "item" => (
                child_text(node, "title").unwrap_or(""),
                child_text(node, "link").unwrap_or("").to_owned(),
                child_text(node, "description").unwrap_or(""),
                child_text(node, "pubDate").or_else(|| child_text(node, "date")),
            ),
            "entry" => (
                child_text(node, "title").unwrap_or(""),
                node.children()
                    .find(|c| c.is_element() && c.tag_name().name() == "link")
                    .and_then(|l| l.attribute("href"))
                    .unwrap_or("")
                    .to_owned(),
                child_text(node, "summary")
                    .or_else(|| child_text(node, "content"))
                    .unwrap_or(""),
                child_text(node, "updated").or_else(|| child_text(node, "published")),
            ),
            _ => continue,
        };
        let mut title = strip_html(title);
        if title.is_empty() {
            continue;
        }
        let mut source = feed.to_owned();
        // Google News titles end " - Publisher"; its description is only a
        // list of the same headlines, so it carries nothing worth keeping.
        let summary = if google {
            if let Some((t, publisher)) = title.rsplit_once(" - ") {
                source = publisher.trim().to_owned();
                title = t.trim().to_owned();
            }
            String::new()
        } else {
            strip_html(summary)
        };
        let url = safe_url(&link);
        out.push(Item {
            id: short_hash(&format!("{url}{title}")),
            kind: "news",
            title,
            summary,
            source,
            url,
            published: published.and_then(rfc3339_from_rfc2822),
            feed,
            ..Default::default()
        });
        if out.len() >= PER_FEED {
            break;
        }
    }
    Ok(out)
}

// ---- hazards -------------------------------------------------------------------

fn parse_json(text: &str) -> std::result::Result<Value, String> {
    serde_json::from_str(text).map_err(|_| "not JSON".to_owned())
}

pub fn parse_usgs(text: &str) -> std::result::Result<Vec<Item>, String> {
    let v = parse_json(text)?;
    let mut out = Vec::new();
    for f in v["features"].as_array().into_iter().flatten() {
        let p = &f["properties"];
        let c = &f["geometry"]["coordinates"];
        let (Some(lon), Some(lat)) = (c[0].as_f64(), c[1].as_f64()) else {
            continue;
        };
        let mag = p["mag"].as_f64().unwrap_or(0.0);
        let place = p["place"].as_str().unwrap_or("").to_owned();
        let depth = c[2]
            .as_f64()
            .map(|d| format!(", {d:.0} km deep"))
            .unwrap_or_default();
        let tsunami = if p["tsunami"].as_i64() == Some(1) {
            " Tsunami flag raised."
        } else {
            ""
        };
        out.push(Item {
            id: format!("usgs-{}", f["id"].as_str().unwrap_or("?")),
            kind: "quake",
            title: p["title"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("M {mag:.1} - {place}")),
            summary: format!("Magnitude {mag:.1} earthquake {place}{depth}.{tsunami}"),
            source: "USGS".to_owned(),
            url: safe_url(p["url"].as_str().unwrap_or("")),
            published: p["time"].as_i64().and_then(rfc3339_from_millis),
            lat: Some(lat),
            lon: Some(lon),
            place: Some(place),
            severity: Some(((mag - 2.5) / 5.0).clamp(0.05, 1.0)),
            feed: "USGS quakes",
            ..Default::default()
        });
    }
    Ok(out)
}

pub fn parse_eonet(text: &str) -> std::result::Result<Vec<Item>, String> {
    let v = parse_json(text)?;
    let mut out = Vec::new();
    for e in v["events"].as_array().into_iter().flatten() {
        let cat = e["categories"][0]["id"].as_str().unwrap_or("");
        let kind = match cat {
            "wildfires" => "fire",
            "severeStorms" => "storm",
            "volcanoes" => "volcano",
            "floods" | "landslides" | "earthquakes" => "alert",
            _ => continue, // sea ice, dust, etc.
        };
        // The latest point (storms carry a track; take where it is now).
        let Some(g) = e["geometry"].as_array().and_then(|g| g.last()) else {
            continue;
        };
        let (lon, lat) = match g["type"].as_str() {
            Some("Point") => (g["coordinates"][0].as_f64(), g["coordinates"][1].as_f64()),
            Some("Polygon") => centroid(&g["coordinates"][0]),
            _ => (None, None),
        };
        let (Some(lon), Some(lat)) = (lon, lat) else {
            continue;
        };
        let magnitude = match (g["magnitudeValue"].as_f64(), g["magnitudeUnit"].as_str()) {
            (Some(m), Some(u)) => format!(" Latest reading: {m:.0} {u}."),
            _ => String::new(),
        };
        let severity = match (kind, g["magnitudeValue"].as_f64()) {
            ("storm", Some(kts)) => (kts / 140.0).clamp(0.2, 1.0),
            ("fire", Some(acres)) => (acres / 100_000.0).clamp(0.2, 1.0),
            _ => 0.4,
        };
        out.push(Item {
            id: format!("eonet-{}", e["id"].as_str().unwrap_or("?")),
            kind,
            title: e["title"].as_str().unwrap_or("Event").to_owned(),
            summary: format!(
                "{} tracked by NASA EONET.{magnitude}",
                e["categories"][0]["title"]
                    .as_str()
                    .unwrap_or("Natural event")
            ),
            source: "NASA EONET".to_owned(),
            url: safe_url(e["sources"][0]["url"].as_str().unwrap_or("")),
            published: g["date"].as_str().map(str::to_owned),
            lat: Some(lat),
            lon: Some(lon),
            severity: Some(severity),
            feed: "NASA EONET",
            ..Default::default()
        });
    }
    Ok(out)
}

pub fn parse_calfire(text: &str) -> std::result::Result<Vec<Item>, String> {
    let v = parse_json(text)?;
    let mut out = Vec::new();
    for f in v.as_array().into_iter().flatten() {
        let (Some(lat), Some(lon)) = (f["Latitude"].as_f64(), f["Longitude"].as_f64()) else {
            continue;
        };
        if lat == 0.0 && lon == 0.0 {
            continue;
        }
        let name = f["Name"].as_str().unwrap_or("Fire").trim().to_owned();
        let county = f["County"].as_str().unwrap_or("").trim();
        let acres = f["AcresBurned"].as_f64();
        let contained = f["PercentContained"].as_f64();
        let mut summary = format!(
            "{} in {} County, California",
            f["Type"].as_str().unwrap_or("Incident"),
            if county.is_empty() {
                "an unnamed"
            } else {
                county
            }
        );
        if let Some(loc) = f["Location"].as_str().filter(|s| !s.trim().is_empty()) {
            summary.push_str(&format!(" ({})", loc.trim()));
        }
        summary.push('.');
        if let Some(a) = acres {
            summary.push_str(&format!(" {a:.0} acres burned."));
        }
        if let Some(c) = contained {
            summary.push_str(&format!(" {c:.0}% contained."));
        }
        if let Some(admin) = f["AdminUnit"].as_str().filter(|s| !s.is_empty()) {
            summary.push_str(&format!(" Lead: {admin}."));
        }
        let uncontained = 1.0 - contained.unwrap_or(0.0) / 100.0;
        out.push(Item {
            id: format!("calfire-{}", f["UniqueId"].as_str().unwrap_or(&name)),
            kind: "fire",
            title: name,
            summary,
            source: "CAL FIRE".to_owned(),
            url: safe_url(f["Url"].as_str().unwrap_or("")),
            published: f["Updated"].as_str().map(str::to_owned),
            lat: Some(lat),
            lon: Some(lon),
            place: Some(format!("{county} County, CA")),
            region: Some("CA".to_owned()),
            severity: Some(
                ((acres.unwrap_or(0.0) / 50_000.0).min(1.0) * 0.6 + uncontained * 0.4)
                    .clamp(0.15, 1.0),
            ),
            feed: "CAL FIRE",
            ..Default::default()
        });
    }
    Ok(out)
}

/// InciWeb's wildfires (prescribed burns skipped), placed from the
/// degrees-minutes-seconds in the description. US only, so west is negative.
pub fn parse_inciweb(xml: &str) -> std::result::Result<Vec<Item>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| "not XML".to_owned())?;
    let mut out = Vec::new();
    for node in doc.descendants().filter(|n| n.has_tag_name("item")) {
        let desc = child_text(node, "description").unwrap_or("");
        if !desc.contains("type of incident is Wildfire") {
            continue;
        }
        let (Some(lat), Some(lon)) = (dms_after(desc, "Latitude:"), dms_after(desc, "Longitude:"))
        else {
            continue;
        };
        let title = strip_html(child_text(node, "title").unwrap_or("Wildfire"));
        let state = desc
            .split("State:")
            .nth(1)
            .and_then(|s| s.lines().map(str::trim).find(|l| !l.is_empty()))
            .unwrap_or("")
            .to_owned();
        let overview = desc
            .split("Incident Overview:")
            .nth(1)
            .map(strip_html)
            .unwrap_or_default();
        let link = child_text(node, "link")
            .unwrap_or("")
            .replacen("http://", "https://", 1);
        out.push(Item {
            id: format!("inciweb-{}", short_hash(&link)),
            kind: "fire",
            title,
            summary: if overview.is_empty() {
                format!("Wildfire in {state}.")
            } else {
                overview
            },
            source: "InciWeb".to_owned(),
            url: safe_url(&link),
            published: desc
                .split("Last updated:")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .map(|d| format!("{d}T12:00:00Z")),
            lat: Some(lat),
            lon: Some(-lon.abs()),
            place: (!state.is_empty()).then_some(state),
            severity: Some(0.45),
            feed: "InciWeb",
            ..Default::default()
        });
    }
    Ok(out)
}

/// `Latitude: 35° 11 8.9` → 35.1858.
fn dms_after(s: &str, label: &str) -> Option<f64> {
    let rest = s.split(label).nth(1)?;
    let nums: Vec<f64> = rest
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .filter(|t| !t.is_empty())
        .take(3)
        .filter_map(|t| t.parse().ok())
        .collect();
    let d = *nums.first()?;
    let m = nums.get(1).copied().unwrap_or(0.0);
    let sec = nums.get(2).copied().unwrap_or(0.0);
    let v = d.abs() + m / 60.0 + sec / 3600.0;
    (v <= 180.0).then_some(if d < 0.0 { -v } else { v })
}

/// Severe and extreme NWS alerts, one per event type and state (a front
/// raises dozens of county alerts). Placed by polygon when there is one,
/// otherwise by the state of its first zone code (`IAZ040` → IA).
pub fn parse_nws(text: &str) -> std::result::Result<Vec<Item>, String> {
    let v = parse_json(text)?;
    let mut grouped: Vec<Item> = Vec::new();
    let mut index: HashMap<(String, String), usize> = HashMap::new();
    let mut counts: Vec<usize> = Vec::new();
    for f in v["features"].as_array().into_iter().flatten() {
        let p = &f["properties"];
        let event = p["event"].as_str().unwrap_or("Alert").to_owned();
        let state = p["geocode"]["UGC"][0]
            .as_str()
            .map(|u| u.chars().take(2).collect::<String>())
            .unwrap_or_default();
        let (lon, lat) = if f["geometry"]["type"].as_str() == Some("Polygon") {
            centroid(&f["geometry"]["coordinates"][0])
        } else {
            (None, None)
        };
        let key = (event.clone(), state.clone());
        if let Some(&i) = index.get(&key) {
            counts[i] += 1;
            if grouped[i].lat.is_none() && lat.is_some() {
                grouped[i].lat = lat;
                grouped[i].lon = lon;
            }
            continue;
        }
        let severity = if p["severity"].as_str() == Some("Extreme") {
            0.9
        } else {
            0.55
        };
        index.insert(key, grouped.len());
        counts.push(1);
        grouped.push(Item {
            id: format!("nws-{}", short_hash(p["id"].as_str().unwrap_or(&event))),
            kind: if is_storm(&event) { "storm" } else { "alert" },
            title: p["headline"].as_str().unwrap_or(&event).to_owned(),
            summary: clip(
                &format!(
                    "{event}. Areas: {}. {}",
                    p["areaDesc"].as_str().unwrap_or(""),
                    p["description"]
                        .as_str()
                        .unwrap_or("")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                MAX_SUMMARY,
            ),
            source: "National Weather Service".to_owned(),
            url: "https://alerts.weather.gov/".to_owned(),
            published: p["sent"].as_str().map(str::to_owned),
            lat,
            lon,
            place: Some(event),
            region: (!state.is_empty()).then_some(state),
            severity: Some(severity),
            feed: "NWS alerts",
            ..Default::default()
        });
    }
    for (item, n) in grouped.iter_mut().zip(counts) {
        if n > 1 {
            item.summary = clip(&format!("{n} active alerts. {}", item.summary), MAX_SUMMARY);
        }
    }
    Ok(grouped)
}

fn is_storm(event: &str) -> bool {
    let e = event.to_lowercase();
    [
        "tornado",
        "thunderstorm",
        "hurricane",
        "tropical",
        "storm",
        "blizzard",
        "wind",
    ]
    .iter()
    .any(|w| e.contains(w))
}

/// Mean of a ring's vertices: close enough to put a dot on.
fn centroid(ring: &Value) -> (Option<f64>, Option<f64>) {
    let pts: Vec<(f64, f64)> = ring
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p[0].as_f64()?, p[1].as_f64()?)))
        .collect();
    if pts.is_empty() {
        return (None, None);
    }
    let n = pts.len() as f64;
    (
        Some(pts.iter().map(|p| p.0).sum::<f64>() / n),
        Some(pts.iter().map(|p| p.1).sum::<f64>() / n),
    )
}

// ---- Wikipedia -------------------------------------------------------------------

pub fn parse_wikipedia(body: &[u8]) -> std::result::Result<Vec<Entry>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "not JSON".to_owned())?;
    let mut pages: Vec<&Value> = v["query"]["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    pages.sort_by_key(|p| p["index"].as_i64().unwrap_or(i64::MAX));
    Ok(pages
        .into_iter()
        .filter_map(|p| {
            let title = p["title"].as_str()?.to_owned();
            let id = p["pageid"].as_i64()?;
            Some(Entry {
                description: p["description"].as_str().unwrap_or("").to_owned(),
                extract: clip(p["extract"].as_str().unwrap_or("").trim(), MAX_SUMMARY),
                url: format!("https://en.wikipedia.org/?curid={id}"),
                title,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_items_are_plain_text_with_safe_links() {
        let xml = r#"<?xml version="1.0"?><rss><channel>
          <item><title>Fire near &lt;b&gt;Malibu&lt;/b&gt;</title>
            <link>https://example.com/a</link>
            <description>&lt;p&gt;Crews &amp;amp; planes &lt;script&gt;x&lt;/script&gt;respond&amp;nbsp;now&lt;/p&gt;</description>
            <pubDate>Fri, 16 Oct 2026 22:35:00 GMT</pubDate></item>
          <item><title>Bad link</title><link>javascript:alert(1)</link></item>
        </channel></rss>"#;
        let items = parse_rss("NPR", xml).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Fire near Malibu");
        assert_eq!(items[0].summary, "Crews & planes x respond now");
        assert_eq!(items[0].url, "https://example.com/a");
        assert_eq!(items[0].published.as_deref(), Some("2026-10-16T22:35:00Z"));
        assert_eq!(items[1].url, "");
    }

    #[test]
    fn google_titles_lose_the_publisher_suffix() {
        let xml = r#"<rss><channel><item><title>Oregon school shooting - The Guardian</title>
          <link>https://news.google.com/x</link><description>&lt;ol&gt;&lt;li&gt;dup&lt;/li&gt;&lt;/ol&gt;</description></item></channel></rss>"#;
        let items = parse_rss("Google News · Top", xml).unwrap();
        assert_eq!(items[0].title, "Oregon school shooting");
        assert_eq!(items[0].source, "The Guardian");
        assert_eq!(items[0].summary, "");
    }

    #[test]
    fn atom_entries_parse() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>Quake felt in Tokyo</title>
          <link href="https://example.com/q"/><summary>Strong shaking</summary>
          <updated>2026-10-16T01:02:03Z</updated></entry></feed>"#;
        let items = parse_rss("Atom", xml).unwrap();
        assert_eq!(items[0].url, "https://example.com/q");
        assert_eq!(items[0].summary, "Strong shaking");
        assert_eq!(items[0].published.as_deref(), Some("2026-10-16T01:02:03Z"));
    }

    #[test]
    fn inciweb_coordinates_are_degrees_minutes_seconds_west() {
        let xml = "<rss><channel><item><title>Ridge Fire</title><link>http://inciweb.wildfire.gov/x</link>\
          <description>Last updated: 2026-10-07\n---\nThe type of incident is Wildfire and involves\n---\nState: Arizona\n---\n\
          Latitude: 35° 11 8.9  Longitude: 111° 40 26 \n---\nIncident Overview: Crews are working.</description></item>\
          <item><title>Burn</title><description>The type of incident is Prescribed Fire Latitude: 1 Longitude: 2</description></item></channel></rss>";
        let items = parse_inciweb(xml).unwrap();
        assert_eq!(items.len(), 1);
        assert!((items[0].lat.unwrap() - 35.1858).abs() < 0.001);
        assert!((items[0].lon.unwrap() + 111.6739).abs() < 0.001);
        assert_eq!(items[0].place.as_deref(), Some("Arizona"));
        assert_eq!(items[0].url, "https://inciweb.wildfire.gov/x");
    }

    #[test]
    fn nws_alerts_group_by_event_and_state() {
        let a = |id: &str, ugc: &str| {
            serde_json::json!({"geometry": null, "properties": {"id": id, "event": "Flood Warning",
              "headline": "Flood Warning", "severity": "Severe", "areaDesc": "Linn", "geocode": {"UGC": [ugc]}}})
        };
        let text =
            serde_json::json!({"features": [a("1", "IAZ040"), a("2", "IAZ041"), a("3", "TXZ001")]})
                .to_string();
        let items = parse_nws(&text).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].region.as_deref(), Some("IA"));
        assert!(items[0].summary.starts_with("2 active alerts."));
    }

    #[test]
    fn wikipedia_entries_keep_search_order() {
        let body = serde_json::json!({"query": {"pages": [
            {"pageid": 2, "index": 2, "title": "Michael Jordan (footballer)", "description": "English footballer"},
            {"pageid": 1, "index": 1, "title": "Michael Jordan", "description": "American basketball player", "extract": "Michael Jeffrey Jordan is..."}
        ]}}).to_string();
        let e = parse_wikipedia(body.as_bytes()).unwrap();
        assert_eq!(e[0].title, "Michael Jordan");
        assert_eq!(e[1].url, "https://en.wikipedia.org/?curid=2");
    }
}
