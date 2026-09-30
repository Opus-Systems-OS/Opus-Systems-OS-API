//! `/v1/usage` — spend rollups the control plane keeps from Anthropic's
//! webhooks: per agent, and the most recent sessions. `export.csv` is the
//! audit trail, oldest first.

use super::AppState;
use crate::auth::middleware::Principal;
use crate::error::{Error, Result};
use axum::body::Body;
use axum::extract::{Extension, Query, State};
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::Json;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
pub struct UsageQuery {
    /// Inclusive lower bound on when the rollup was observed: RFC 3339, or
    /// `YYYY-MM-DD` for midnight UTC.
    pub since: Option<String>,
    /// Exclusive upper bound, same forms.
    pub until: Option<String>,
}

impl UsageQuery {
    fn pairs(&self) -> Vec<(&str, &str)> {
        let mut q = Vec::new();
        if let Some(s) = &self.since {
            q.push(("since", s.as_str()));
        }
        if let Some(u) = &self.until {
            q.push(("until", u.as_str()));
        }
        q
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct AgentUsage {
    pub agent_slug: String,
    pub session_count: u64,
    pub total_list_cost_cents: u64,
    pub budget_reached_count: u64,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct SessionUsage {
    pub session_id: String,
    pub agent_slug: String,
    pub environment_slug: Option<String>,
    /// Whole US cents as a string; null until a webhook has reported it.
    pub list_cost_cents: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub active_seconds: Option<f64>,
    pub budget_reached: bool,
    pub last_event_type: String,
    pub observed_at: String,
    /// `"<type>: <message>"` when the session's last turn ended on a
    /// `session.error` (billing, upstream outage) instead of a reply.
    pub last_error: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct UsageWindow {
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Serialize, utoipa::ToSchema)]
pub struct Usage {
    pub window: UsageWindow,
    pub by_agent: Vec<AgentUsage>,
    /// Most recently observed first, at most 100.
    pub recent: Vec<SessionUsage>,
}

#[utoipa::path(get, path = "/usage", tag = "usage", security(("api_key" = ["usage:read"])),
    params(UsageQuery),
    responses(
        (status = 200, body = Usage),
        (status = 400, description = "bad since/until", body = crate::openapi::ErrorBody),
    ))]
pub async fn get(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Value>> {
    let mut usage = state.control_plane.get("/usage", &q.pairs()).await?;
    // A key limited to some agents sees only their spend.
    if who.agents.is_some() {
        for part in ["by_agent", "recent"] {
            if let Some(rows) = usage.get_mut(part) {
                super::access::retain_reachable(&who, rows, |r| r["agent_slug"].as_str());
            }
        }
    }
    Ok(Json(usage))
}

#[utoipa::path(get, path = "/usage/export.csv", tag = "usage", security(("api_key" = ["usage:read"])),
    params(UsageQuery),
    responses(
        (status = 200, description = "RFC 4180 CSV, oldest first; header row `session_id,agent_slug,…,last_error`", content_type = "text/csv"),
    ))]
pub async fn export_csv(
    State(state): State<AppState>,
    Extension(who): Extension<Principal>,
    Query(q): Query<UsageQuery>,
) -> Result<Response> {
    let upstream = state
        .control_plane
        .open::<Value>(Method::GET, "/usage/export.csv", &q.pairs(), None)
        .await?;
    let disposition = upstream.headers().get(header::CONTENT_DISPOSITION).cloned();
    let mut res = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8");
    if let Some(d) = disposition {
        res = res.header(header::CONTENT_DISPOSITION, d);
    }
    let body = if who.agents.is_some() {
        // A key limited to some agents gets only their rows: the whole
        // export is read (it is a few hundred rows) and filtered.
        let csv = upstream
            .text()
            .await
            .map_err(|e| Error::Config(format!("csv body: {e}")))?;
        Body::from(retain_rows(&csv, |agent| who.may_reach(agent)))
    } else {
        Body::from_stream(upstream.bytes_stream())
    };
    res.body(body)
        .map_err(|e| Error::Config(format!("csv response: {e}")))
}

/// The CSV's header and the rows whose `agent_slug` passes `keep`. Records
/// are RFC 4180: a quoted field (an error message) may hold commas, quotes
/// and line breaks.
fn retain_rows(csv: &str, keep: impl Fn(&str) -> bool) -> String {
    let mut records = records(csv).into_iter();
    let Some((header, names)) = records.next() else {
        return String::new();
    };
    let Some(col) = names.iter().position(|n| n == "agent_slug") else {
        return format!("{header}\n");
    };
    let mut out = format!("{header}\n");
    for (raw, fields) in records {
        if fields.get(col).is_some_and(|a| keep(a)) {
            out.push_str(raw);
            out.push('\n');
        }
    }
    out
}

/// Each record's raw text (line break excluded) and its unquoted fields.
fn records(csv: &str) -> Vec<(&str, Vec<String>)> {
    let mut out = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, b) in csv.bytes().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b'\n' if !quoted => {
                let raw = csv[start..i].trim_end_matches('\r');
                if !raw.is_empty() {
                    out.push((raw, fields(raw)));
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let rest = csv[start..].trim_end_matches('\r');
    if !rest.is_empty() {
        out.push((rest, fields(rest)));
    }
    out
}

fn fields(record: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = record.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get))
        .routes(routes!(export_csv))
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_are_kept_by_agent_even_with_quoted_line_breaks() {
        let csv = "session_id,agent_slug,last_error\n\
                   sesn_1,jarvis,\n\
                   sesn_2,jarvis-powers,\"a, \"\"quoted\"\"\nerror\"\r\n\
                   sesn_3,blueweb-client,x\n";
        let kept = super::retain_rows(csv, |a| a == "jarvis-powers");
        assert_eq!(
            kept,
            "session_id,agent_slug,last_error\nsesn_2,jarvis-powers,\"a, \"\"quoted\"\"\nerror\"\n"
        );
        assert_eq!(super::retain_rows("", |_| true), "");
    }
}
