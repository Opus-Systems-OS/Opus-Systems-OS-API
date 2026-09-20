//! The Docker engine on the droplet, over its unix socket (mounted
//! read-only into the API's container). One call: `GET /containers/json`.

use super::{plural, Report, State};
use crate::error::{Error, Result};
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::path::Path;

pub async fn check(socket: &Path) -> Result<Report> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| Error::Upstream {
            status: 503,
            kind: "ops".into(),
            message: format!("Docker: socket {}: {e}", socket.display()),
            retry_after: None,
        })?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| docker_err(format!("handshake: {e}")))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let req = hyper::Request::builder()
        .uri("/containers/json?all=1")
        .header(hyper::header::HOST, "docker")
        .body(Empty::<Bytes>::new())
        .map_err(|e| docker_err(e.to_string()))?;
    let res = sender
        .send_request(req)
        .await
        .map_err(|e| docker_err(format!("request: {e}")))?;
    let status = res.status();
    let body = res
        .into_body()
        .collect()
        .await
        .map_err(|e| docker_err(format!("body: {e}")))?
        .to_bytes();
    if !status.is_success() {
        return Err(docker_err(format!(
            "{} {}",
            status.as_u16(),
            String::from_utf8_lossy(&body)
        )));
    }
    let list: Vec<Value> =
        serde_json::from_slice(&body).map_err(|e| docker_err(format!("bad JSON: {e}")))?;
    let containers: Vec<Value> = list
        .into_iter()
        .map(|c| {
            let name = c
                .pointer("/Names/0")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_start_matches('/')
                .to_owned();
            let ports: Vec<String> = c
                .get("Ports")
                .and_then(Value::as_array)
                .map(|ps| {
                    ps.iter()
                        .filter_map(|p| {
                            let private = p.get("PrivatePort").and_then(Value::as_i64)?;
                            let proto = p.get("Type").and_then(Value::as_str).unwrap_or("tcp");
                            Some(match p.get("PublicPort").and_then(Value::as_i64) {
                                Some(public) => format!("{public}->{private}/{proto}"),
                                None => format!("{private}/{proto}"),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "name": name,
                "image": c.get("Image").and_then(Value::as_str).unwrap_or(""),
                "state": c.get("State").and_then(Value::as_str).unwrap_or(""),
                "status": c.get("Status").and_then(Value::as_str).unwrap_or(""),
                "ports": ports,
            })
        })
        .collect();
    let running = containers
        .iter()
        .filter(|c| c["state"] == "running")
        .count();
    let state = if containers.is_empty() {
        State::Unknown
    } else if running == containers.len() {
        State::Ok
    } else if running == 0 {
        State::Down
    } else {
        State::Warn
    };
    Ok(Report {
        state,
        headline: if containers.is_empty() {
            plural(0, "container", "containers")
        } else {
            format!("{running}/{} running", containers.len())
        },
        detail: json!({ "containers": containers }),
    })
}

fn docker_err(message: String) -> Error {
    Error::Upstream {
        status: 502,
        kind: "ops".into(),
        message: format!("Docker: {message}"),
        retry_after: None,
    }
}
