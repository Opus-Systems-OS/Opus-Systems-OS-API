//! `/v1/sources*` and `/v1/briefing` (`sources:read`): the day's weather,
//! mail, calendar, YouTube, WHOOP and Buffer queue, each a row like
//! `/v1/ops` — a source that can't be reached is `down` with the reason.
//! `/v1/oauth/{provider}/callback` is public: it is where Google and WHOOP
//! send the browser back after consent, guarded by a one-time state.

use super::AppState;
use crate::error::{Error, Result};
use crate::upstream::ops::Status;
use crate::upstream::sources::oauth::{self, Provider};
use axum::extract::{Path, Query, State};
use axum::response::Html;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Serialize, utoipa::ToSchema)]
pub struct SourceList {
    /// The configured sources, briefing order, without detail.
    pub sources: Vec<Status>,
}

#[utoipa::path(get, path = "/sources", tag = "sources", security(("api_key" = ["sources:read"])),
    responses((status = 200, body = SourceList)))]
pub async fn list(State(state): State<AppState>) -> Result<Json<SourceList>> {
    let sources = state.sources.as_ref().as_ref().ok_or(Error::NotFound)?;
    let mut rows = sources.all().await;
    for r in &mut rows {
        r.detail = None;
    }
    Ok(Json(SourceList { sources: rows }))
}

#[utoipa::path(get, path = "/sources/{source}", tag = "sources", security(("api_key" = ["sources:read"])),
    params(("source" = String, Path, description = "`weather`, `calendar`, `gmail`, `youtube`, `whoop` or `buffer`")),
    responses(
        (status = 200, body = Status, description = "The source with its `detail` document"),
        (status = 404, body = crate::openapi::ErrorBody, description = "Unknown, or not configured on this API"),
    ))]
pub async fn one(
    State(state): State<AppState>,
    Path(source): Path<String>,
) -> Result<Json<Status>> {
    let sources = state.sources.as_ref().as_ref().ok_or(Error::NotFound)?;
    Ok(Json(sources.status(&source).await?))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct BriefingQuery {
    /// RFC 3339: the client's last visit. Gmail's `new_since` counts unread
    /// messages that arrived after it.
    pub since: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Briefing {
    pub generated_at: String,
    pub since: Option<String>,
    /// Every configured source with its detail, briefing order.
    pub sources: Vec<Status>,
}

#[utoipa::path(get, path = "/briefing", tag = "sources", security(("api_key" = ["sources:read"])),
    params(BriefingQuery),
    responses((status = 200, body = Briefing)))]
pub async fn briefing(
    State(state): State<AppState>,
    Query(q): Query<BriefingQuery>,
) -> Result<Json<Briefing>> {
    let sources = state.sources.as_ref().as_ref().ok_or(Error::NotFound)?;
    let since = q
        .since
        .map(|s| {
            time::OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
                .map(|_| s)
                .map_err(|_| Error::InvalidRequest("since must be an RFC 3339 time".into()))
        })
        .transpose()?;
    let mut rows = sources.all().await;
    if let Some(since) = &since {
        for r in rows.iter_mut().filter(|r| r.id == "gmail") {
            if let Some(d) = r.detail.as_mut() {
                let n = new_since(d, since);
                d["new_since"] = Value::from(n);
            }
        }
    }
    Ok(Json(Briefing {
        generated_at: crate::db::now(),
        since,
        sources: rows,
    }))
}

/// How many of Gmail's recent unread arrived after `since` (RFC 3339).
pub fn new_since(gmail_detail: &Value, since: &str) -> usize {
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    };
    let Some(since) = parse(since) else { return 0 };
    gmail_detail["recent"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["at"].as_str().and_then(parse))
                .filter(|at| *at > since)
                .count()
        })
        .unwrap_or(0)
}

#[derive(Deserialize, utoipa::IntoParams)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    /// Set by the provider when consent was refused.
    pub error: Option<String>,
}

#[utoipa::path(get, path = "/oauth/{provider}/callback", tag = "sources",
    params(("provider" = String, Path, description = "`google` or `whoop`"), CallbackQuery),
    responses(
        (status = 200, description = "Connected; a short page to close", content_type = "text/html"),
        (status = 400, body = crate::openapi::ErrorBody, description = "refused, or an unknown/used/expired state"),
    ))]
pub async fn callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(q): Query<CallbackQuery>,
) -> Result<Html<String>> {
    let sources = state.sources.as_ref().as_ref().ok_or(Error::NotFound)?;
    let p = Provider::parse(&provider).ok_or(Error::NotFound)?;
    if let Some(err) = q.error {
        return Err(Error::InvalidRequest(format!(
            "{} consent was not given: {err}",
            p.name()
        )));
    }
    let (Some(code), Some(st)) = (q.code, q.state) else {
        return Err(Error::InvalidRequest("missing code or state".into()));
    };
    oauth::finish(sources, p, &code, &st).await?;
    Ok(Html(format!(
        "<!doctype html><meta charset=utf-8><title>Connected</title>\
         <body style=\"font:16px system-ui;background:#0b0603;color:#ffe6cc;padding:3rem\">\
         <h1 style=\"color:#ff7a1a\">{} connected</h1><p>Jarvis can read it now. You can close this tab.</p>",
        p.name()
    )))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(one))
        .routes(routes!(briefing))
}

/// No key: the provider's redirect carries none. The one-time state is the
/// guard.
pub fn open_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(callback))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_mail_is_counted_after_the_last_visit() {
        let d = json!({"recent": [
            {"at": "2026-09-28T08:00:00Z"}, {"at": "2026-09-28T10:30:00Z"}, {"at": null}
        ]});
        assert_eq!(new_since(&d, "2026-09-28T09:00:00Z"), 1);
        assert_eq!(new_since(&d, "2026-09-27T00:00:00Z"), 2);
        assert_eq!(new_since(&d, "garbage"), 0);
    }
}
