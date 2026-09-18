//! Stage 3 exit test / reference client: drive one jarvis turn over the
//! WebSocket. Exactly what a headset does, minus the headset.
//!
//!   OPUS_API_KEY=osk_… cargo run --example ws_drive -- wss://api.opustower.dev sesn_…
//!
//! Connects with `?history=false`, waits for `hello`, sends a `message`,
//! prints every frame until the turn's `session.status_idle`, then sends
//! `ping`, expects `pong`, and closes.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let base = args.next().expect("base url, e.g. wss://api.opustower.dev");
    let session = args.next().expect("session id");
    let key = std::env::var("OPUS_API_KEY").expect("OPUS_API_KEY");
    let task = args
        .next()
        .unwrap_or_else(|| "Reply with one short sentence: what is 17 times 23?".to_owned());

    let url = format!("{base}/v1/sessions/{session}/ws?history=false");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("upgrade");

    let hello = next(&mut ws).await;
    assert_eq!(hello["type"], "hello", "{hello}");
    println!(
        "hello   session={} request_id={}",
        hello["session_id"], hello["request_id"]
    );

    ws.send(Message::Text(
        json!({"type": "message", "task": task}).to_string().into(),
    ))
    .await
    .unwrap();

    let started = std::time::Instant::now();
    loop {
        let f = next(&mut ws).await;
        match f["type"].as_str() {
            Some("sent") => println!("sent    {}", f["data"][0]["type"]),
            Some("event") => {
                let ev = &f["event"];
                let t = ev["type"].as_str().unwrap_or("");
                match t {
                    "agent.message" => {
                        let text: String = ev["content"]
                            .as_array()
                            .map(|c| c.iter().filter_map(|b| b["text"].as_str()).collect())
                            .unwrap_or_default();
                        println!("event   {t}: {text}");
                    }
                    "session.status_idle" => {
                        println!(
                            "event   {t} ({}) after {:.1}s",
                            ev["stop_reason"]["type"],
                            started.elapsed().as_secs_f32()
                        );
                        break;
                    }
                    _ if t.starts_with("span.") => {}
                    _ => println!("event   {t}"),
                }
            }
            Some("error") => {
                eprintln!("error   {}", f["error"]);
                std::process::exit(1);
            }
            Some("closed") => {
                eprintln!("closed  {}", f["reason"]);
                std::process::exit(1);
            }
            _ => println!("frame   {f}"),
        }
    }

    ws.send(Message::Text(json!({"type": "ping"}).to_string().into()))
        .await
        .unwrap();
    let pong = next(&mut ws).await;
    assert_eq!(pong["type"], "pong", "{pong}");
    println!("pong");
    ws.close(None).await.unwrap();
    println!("closed cleanly");
}

async fn next(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(120), ws.next())
            .await
            .expect("frame within 120s")
            .expect("socket open")
            .expect("frame ok")
        {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Close(c) => panic!("closed: {c:?}"),
            _ => {}
        }
    }
}
