//! GitHub, for one organisation: each repo's open pull requests and latest
//! workflow run, plus the token owner's unread notifications. Fine-grained
//! token with Contents / Pull requests / Actions / Metadata read on the
//! org's repos and Notifications read.

use super::{get_json, plural, Report, State};
use crate::error::Result;
use serde_json::{json, Value};

/// Repos beyond this many (by last push) are listed without PR/CI detail,
/// to keep one refresh under twenty calls.
const DETAILED_REPOS: usize = 8;

pub async fn check(http: &reqwest::Client, base: &str, token: &str, org: &str) -> Result<Report> {
    let repos_body = get_json(
        http,
        &format!("{base}/orgs/{org}/repos?per_page=50&sort=pushed"),
        token,
        "GitHub",
    )
    .await?;
    let mut repos = Vec::new();
    let mut open_prs = 0usize;
    let mut failing = Vec::new();
    for (i, r) in repos_body
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        let full = r
            .get("full_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let mut entry = json!({
            "name": name,
            "full_name": full,
            "private": r.get("private").and_then(Value::as_bool).unwrap_or(false),
            "pushed_at": r.get("pushed_at").and_then(Value::as_str).unwrap_or(""),
            "default_branch": r.get("default_branch").and_then(Value::as_str).unwrap_or(""),
        });
        if i < DETAILED_REPOS {
            let pulls = get_json(
                http,
                &format!("{base}/repos/{full}/pulls?state=open&per_page=10"),
                token,
                "GitHub",
            )
            .await?;
            let prs: Vec<Value> = pulls
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    json!({
                        "number": p.get("number").and_then(Value::as_i64).unwrap_or(0),
                        "title": p.get("title").and_then(Value::as_str).unwrap_or(""),
                        "author": p.pointer("/user/login").and_then(Value::as_str).unwrap_or(""),
                        "draft": p.get("draft").and_then(Value::as_bool).unwrap_or(false),
                        "updated_at": p.get("updated_at").and_then(Value::as_str).unwrap_or(""),
                        "url": p.get("html_url").and_then(Value::as_str).unwrap_or(""),
                    })
                })
                .collect();
            open_prs += prs.len();
            let runs = get_json(
                http,
                &format!("{base}/repos/{full}/actions/runs?per_page=1"),
                token,
                "GitHub",
            )
            .await?;
            let run = runs.pointer("/workflow_runs/0").map(|w| {
                json!({
                    "name": w.get("name").and_then(Value::as_str).unwrap_or(""),
                    "status": w.get("status").and_then(Value::as_str).unwrap_or(""),
                    "conclusion": w.get("conclusion").and_then(Value::as_str),
                    "branch": w.get("head_branch").and_then(Value::as_str).unwrap_or(""),
                    "updated_at": w.get("updated_at").and_then(Value::as_str).unwrap_or(""),
                    "url": w.get("html_url").and_then(Value::as_str).unwrap_or(""),
                })
            });
            if let Some(run) = &run {
                if matches!(
                    run["conclusion"].as_str(),
                    Some("failure") | Some("timed_out") | Some("startup_failure")
                ) {
                    failing.push(name.clone());
                }
            }
            entry["open_prs"] = json!(prs);
            entry["last_run"] = run.unwrap_or(Value::Null);
        }
        repos.push(entry);
    }
    let notes_body = get_json(
        http,
        &format!("{base}/notifications?per_page=10"),
        token,
        "GitHub",
    )
    .await?;
    let notifications: Vec<Value> = notes_body
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|n| {
            json!({
                "title": n.pointer("/subject/title").and_then(Value::as_str).unwrap_or(""),
                "type": n.pointer("/subject/type").and_then(Value::as_str).unwrap_or(""),
                "repo": n.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or(""),
                "reason": n.get("reason").and_then(Value::as_str).unwrap_or(""),
                "updated_at": n.get("updated_at").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();
    let state = if repos.is_empty() {
        State::Unknown
    } else if failing.is_empty() {
        State::Ok
    } else {
        State::Warn
    };
    let ci = if failing.is_empty() {
        "CI green".to_owned()
    } else {
        format!("CI failing: {}", failing.join(", "))
    };
    let headline = format!(
        "{} · {} · {}",
        plural(open_prs, "open PR", "open PRs"),
        ci,
        plural(notifications.len(), "notification", "notifications")
    );
    Ok(Report {
        state,
        headline,
        detail: json!({ "org": org, "repos": repos, "notifications": notifications }),
    })
}
