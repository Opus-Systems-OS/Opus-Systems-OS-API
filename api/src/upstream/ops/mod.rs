//! The stack's services, watched read-only: UptimeRobot, Tailscale,
//! Cloudflare, Docker on the droplet, the droplet itself (DigitalOcean) and
//! GitHub. Each service turns its token into a one-line headline, a
//! traffic-light state and a detail document; the tokens stay here.
//!
//! Every service is polled at most once per `TTL` no matter how many panels
//! ask, and a service that cannot be reached reports `down` with the reason
//! rather than failing the whole hub.

pub mod cloudflare;
pub mod digitalocean;
pub mod docker;
pub mod github;
pub mod tailscale;
pub mod uptimerobot;

use crate::config::OpsConfig;
use crate::error::{Error, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// How long one poll of a service is reused.
pub const TTL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Everything the service reports is healthy.
    Ok,
    /// Something needs a look (a paused monitor, a failed CI run, a stopped container).
    Warn,
    /// The service reports an outage, or could not be reached at all.
    Down,
    /// Nothing to judge yet.
    Unknown,
}

/// One service's poll result, before it is stamped and cached.
#[derive(Debug, Clone)]
pub struct Report {
    pub state: State,
    pub headline: String,
    pub detail: Value,
}

/// A service as the hub lists it. `detail` is only filled on the
/// single-service route.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct Status {
    /// `uptimerobot`, `tailscale`, `cloudflare`, `docker`, `droplet`, `github`.
    pub id: String,
    /// Display name.
    pub name: String,
    pub state: State,
    /// One line for a list row.
    pub headline: String,
    /// RFC 3339, when this was last fetched from the service.
    pub checked_at: String,
    /// Service-specific; see each service's docs.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub detail: Option<Value>,
}

/// Overridable base URLs so tests can point every service at a stub.
#[derive(Debug, Clone)]
pub struct BaseUrls {
    pub uptimerobot: String,
    pub tailscale: String,
    pub cloudflare: String,
    pub digitalocean: String,
    pub github: String,
}

impl Default for BaseUrls {
    fn default() -> Self {
        BaseUrls {
            uptimerobot: "https://api.uptimerobot.com".into(),
            tailscale: "https://api.tailscale.com".into(),
            cloudflare: "https://api.cloudflare.com".into(),
            digitalocean: "https://api.digitalocean.com".into(),
            github: "https://api.github.com".into(),
        }
    }
}

#[derive(Clone)]
pub struct Ops {
    http: reqwest::Client,
    cfg: OpsConfig,
    urls: BaseUrls,
    cache: Arc<Mutex<HashMap<&'static str, (Instant, Status)>>>,
}

/// The services, in the order the hub shows them.
const SERVICES: [(&str, &str); 6] = [
    ("github", "GitHub"),
    ("uptimerobot", "UptimeRobot"),
    ("droplet", "Droplet"),
    ("docker", "Docker"),
    ("tailscale", "Tailscale"),
    ("cloudflare", "Cloudflare"),
];

impl Ops {
    pub fn new(cfg: OpsConfig) -> Result<Self> {
        Self::with_base_urls(cfg, BaseUrls::default())
    }

    pub fn with_base_urls(cfg: OpsConfig, urls: BaseUrls) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("opus-api/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Ops {
            http,
            cfg,
            urls,
            cache: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// The configured services' ids, hub order.
    pub fn configured(&self) -> Vec<&'static str> {
        SERVICES
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| self.is_configured(id))
            .collect()
    }

    fn is_configured(&self, id: &str) -> bool {
        match id {
            "uptimerobot" => self.cfg.uptimerobot_api_key.is_some(),
            "tailscale" => self.cfg.tailscale_api_key.is_some(),
            "cloudflare" => self.cfg.cloudflare_api_token.is_some(),
            "droplet" => self.cfg.digitalocean_token.is_some(),
            "github" => self.cfg.github_token.is_some(),
            "docker" => self.cfg.docker_socket.is_some(),
            _ => false,
        }
    }

    /// Every configured service, polled concurrently, without detail.
    pub async fn summary(&self) -> Vec<Status> {
        let ids = self.configured();
        let polls = ids.iter().map(|id| self.status(id));
        futures_util::future::join_all(polls)
            .await
            .into_iter()
            .flatten()
            .map(|mut s| {
                s.detail = None;
                s
            })
            .collect()
    }

    /// One service with its detail. `Err(NotFound)` for an unknown or
    /// unconfigured id.
    pub async fn status(&self, id: &str) -> Result<Status> {
        let (id, name) = SERVICES
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
        let report = match self.poll(id).await {
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
            checked_at: now(),
            detail: Some(report.detail),
        };
        self.cache
            .lock()
            .await
            .insert(id, (Instant::now(), status.clone()));
        Ok(status)
    }

    async fn poll(&self, id: &str) -> Result<Report> {
        match id {
            "uptimerobot" => {
                uptimerobot::check(
                    &self.http,
                    &self.urls.uptimerobot,
                    self.cfg.uptimerobot_api_key.as_deref().unwrap_or_default(),
                )
                .await
            }
            "tailscale" => {
                tailscale::check(
                    &self.http,
                    &self.urls.tailscale,
                    self.cfg.tailscale_api_key.as_deref().unwrap_or_default(),
                    &self.cfg.tailscale_tailnet,
                )
                .await
            }
            "cloudflare" => {
                cloudflare::check(
                    &self.http,
                    &self.urls.cloudflare,
                    self.cfg.cloudflare_api_token.as_deref().unwrap_or_default(),
                )
                .await
            }
            "droplet" => {
                digitalocean::check(
                    &self.http,
                    &self.urls.digitalocean,
                    self.cfg.digitalocean_token.as_deref().unwrap_or_default(),
                )
                .await
            }
            "github" => {
                github::check(
                    &self.http,
                    &self.urls.github,
                    self.cfg.github_token.as_deref().unwrap_or_default(),
                    &self.cfg.github_org,
                )
                .await
            }
            "docker" => {
                docker::check(
                    self.cfg
                        .docker_socket
                        .as_deref()
                        .unwrap_or_else(|| std::path::Path::new("")),
                )
                .await
            }
            _ => Err(Error::NotFound),
        }
    }
}

/// Why a poll failed, as the hub row shows it. Never a token, never a URL
/// with a query string.
fn reason(e: &Error) -> String {
    match e {
        Error::Upstream { message, .. } => message.clone(),
        Error::UpstreamTransport(t) => {
            let what = if t.is_timeout() {
                "timed out"
            } else if t.is_connect() {
                "could not connect"
            } else {
                "no usable response"
            };
            what.to_owned()
        }
        other => other.to_string(),
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// A GET with a bearer, decoded as JSON; a non-2xx becomes an upstream
/// error carrying the status, so the hub can say "GitHub: 401".
pub(crate) async fn get_json(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    service: &str,
) -> Result<Value> {
    let res = http.get(url).bearer_auth(bearer).send().await?;
    read_json(res, service).await
}

pub(crate) async fn read_json(res: reqwest::Response, service: &str) -> Result<Value> {
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .or_else(|| v.get("error"))
                    .and_then(|m| m.as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_owned());
        return Err(Error::Upstream {
            status: status.as_u16(),
            kind: "ops".into(),
            message: format!("{service}: {} {message}", status.as_u16()),
            retry_after: None,
        });
    }
    Ok(res.json::<Value>().await?)
}

/// `"2026-09-19T12:00:00Z"` → how long ago, for headlines.
pub(crate) fn ago(rfc3339: &str) -> Option<Duration> {
    let then = time::OffsetDateTime::parse(rfc3339, &time::format_description::well_known::Rfc3339)
        .ok()?;
    let delta = time::OffsetDateTime::now_utc() - then;
    Some(Duration::from_secs(delta.whole_seconds().max(0) as u64))
}

pub(crate) fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}
