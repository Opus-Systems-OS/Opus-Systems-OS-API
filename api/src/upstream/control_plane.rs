//! HTTP client to Iron-Fleet's control plane — the API's one upstream in
//! stage 2. Authenticates with `CONTROL_PLANE_TOKEN`; a client's key never
//! travels further than this process.
//!
//! Two reqwest clients: `json` has a 30 s total timeout and is what every
//! request/response call uses; `stream` has only a connect timeout, because
//! an SSE session stream or an NDJSON chat stream is open for as long as the
//! caller keeps it open. Both share the bearer.
//!
//! A non-2xx is reduced to `Error::Upstream { status, kind, message }` from
//! the control plane's `{"error":{"type","message"}}` body; `error.rs`
//! decides what the client sees. A 2xx streaming response is handed back
//! whole so the handler can forward its bytes untouched.

use crate::error::{Error, Result};
use reqwest::{Method, Response, StatusCode};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct ControlPlane {
    json: reqwest::Client,
    stream: reqwest::Client,
    base_url: String,
    token: String,
}

impl ControlPlane {
    pub fn new(base_url: &str, token: &str) -> Result<Self> {
        let ua = concat!("opus-api/", env!("CARGO_PKG_VERSION"));
        let json = reqwest::Client::builder()
            .user_agent(ua)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()?;
        let stream = reqwest::Client::builder()
            .user_agent(ua)
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(ControlPlane {
            json,
            stream,
            base_url: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET path?query` → parsed JSON.
    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let res = self
            .send(&self.json, Method::GET, path, query, None::<&()>)
            .await?;
        Ok(res.json().await?)
    }

    /// `POST path` with a JSON body → `(status, parsed JSON)`. The status is
    /// returned because the control plane distinguishes 201 (created) from
    /// 200, and so do we.
    pub async fn post<B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(StatusCode, Value)> {
        let res = self
            .send(&self.json, Method::POST, path, &[], Some(body))
            .await?;
        let status = res.status();
        Ok((status, res.json().await?))
    }

    /// A 2xx response whose body the caller streams (SSE, NDJSON, CSV).
    /// Status is already checked.
    pub async fn open<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&B>,
    ) -> Result<Response> {
        self.send(&self.stream, method, path, query, body).await
    }

    /// `GET path` with the timeout-bearing client, but returning the raw
    /// response *including* non-2xx — for the one route (`/v1/rig`) whose
    /// answer is composed from the upstream status rather than mapped.
    pub async fn get_raw(&self, path: &str) -> Result<Response> {
        Ok(self
            .json
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .send()
            .await?)
    }

    async fn send<B: Serialize + ?Sized>(
        &self,
        client: &reqwest::Client,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&B>,
    ) -> Result<Response> {
        let mut req = client
            .request(method, format!("{}{}", self.base_url, path))
            .bearer_auth(&self.token)
            .query(query);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req.send().await?;
        if res.status().is_success() {
            return Ok(res);
        }
        Err(upstream_error(res).await)
    }
}

/// Reduce a non-2xx control-plane response to `Error::Upstream`.
async fn upstream_error(res: Response) -> Error {
    let status = res.status().as_u16();
    let retry_after = res
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok());
    let body: Value = res.json().await.unwrap_or(Value::Null);
    let kind = body["error"]["type"]
        .as_str()
        .unwrap_or("upstream")
        .to_owned();
    let message = body["error"]["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("upstream error")
                .to_owned()
        });
    Error::Upstream {
        status,
        kind,
        message,
        retry_after,
    }
}
