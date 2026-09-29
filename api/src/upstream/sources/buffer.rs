//! Buffer (GraphQL at `api.buffer.com`, bearer API key): what is queued to
//! post. Posts are per organization, so the account's organizations are
//! read first, then each one's scheduled posts, soonest first.

use super::{source_error, Sources};
use crate::error::Result;
use crate::upstream::ops::{plural, read_json, Report, State};
use serde_json::{json, Value};

const ORGS: &str = "query { account { organizations { id name } } }";

/// Buffer's documented query, with the organization id inlined as a JSON
/// string literal (its example does the same; no variable type to guess).
fn scheduled(org: &str) -> String {
    format!(
        "query {{ posts(input: {{organizationId: {}, sort: [{{field: dueAt, direction: asc}}], filter: {{status: [scheduled]}}}}) {{ edges {{ node {{ id text dueAt }} }} }} }}",
        Value::String(org.to_owned())
    )
}

async fn graphql(s: &Sources, key: &str, query: &str, variables: Value) -> Result<Value> {
    let res = s
        .http
        .post(&s.urls.buffer)
        .bearer_auth(key)
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await?;
    let body = read_json(res, "Buffer").await?;
    if let Some(msg) = body.pointer("/errors/0/message").and_then(Value::as_str) {
        return Err(source_error("Buffer", msg));
    }
    Ok(body)
}

pub async fn check(s: &Sources) -> Result<Report> {
    let key = s.cfg.buffer_api_key.as_deref().unwrap_or_default();
    let orgs = graphql(s, key, ORGS, json!({})).await?;
    let ids: Vec<String> = orgs
        .pointer("/data/account/organizations")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| o["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut posts = Vec::new();
    for id in ids {
        let page = graphql(s, key, &scheduled(&id), json!({})).await?;
        posts.extend(queued(&page));
    }
    posts.sort_by(|a, b| a["due_at"].as_str().cmp(&b["due_at"].as_str()));
    Ok(report(posts))
}

/// A `posts` page → `[{text, due_at}]`, text cut to one line.
pub fn queued(page: &Value) -> Vec<Value> {
    page.pointer("/data/posts/edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .map(|e| {
                    let text = e
                        .pointer("/node/text")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let line: String = text
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(120)
                        .collect();
                    json!({ "text": line, "due_at": e.pointer("/node/dueAt") })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn report(posts: Vec<Value>) -> Report {
    Report {
        state: if posts.is_empty() {
            State::Warn
        } else {
            State::Ok
        },
        headline: if posts.is_empty() {
            "queue empty".into()
        } else {
            format!("{} queued", plural(posts.len(), "post", "posts"))
        },
        detail: json!({ "queued": posts.len(), "next": posts.iter().take(5).collect::<Vec<_>>() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_id_is_quoted_into_the_query() {
        let q = scheduled("org\"1");
        assert!(q.contains(r#"organizationId: "org\"1""#), "{q}");
        assert!(q.contains("filter: {status: [scheduled]}"));
    }

    #[test]
    fn scheduled_posts_become_a_row() {
        let page = json!({"data": {"posts": {"edges": [
            {"node": {"id": "p1", "text": "New short is up!\nLink below", "dueAt": "2026-09-29T17:00:00Z"}}
        ]}}});
        let posts = queued(&page);
        assert_eq!(posts[0]["text"], "New short is up!");
        let r = report(posts);
        assert_eq!(r.headline, "1 post queued");
        assert_eq!(report(vec![]).headline, "queue empty");
    }
}
