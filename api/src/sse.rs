//! A small Server-Sent Events parser over a byte stream: enough of the spec
//! for the control plane's session stream (`: connected` comment first, then
//! `event: message` + `data: {json}` frames, blank-line separated). Used by
//! the WebSocket bridge, which needs the events as values, not bytes.

use bytes::{Bytes, BytesMut};

/// Incremental parser: feed bytes, take complete frames.
#[derive(Default)]
pub struct SseParser {
    buf: BytesMut,
}

/// One frame's meaningful parts. Comments and unknown fields are dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

impl SseParser {
    pub fn push(&mut self, chunk: &Bytes) {
        self.buf.extend_from_slice(chunk);
    }

    /// Complete frames currently in the buffer. A frame ends at a blank line;
    /// whatever follows the last blank line is kept for the next `push`.
    pub fn frames(&mut self) -> Vec<SseFrame> {
        let mut out = Vec::new();
        loop {
            let text = String::from_utf8_lossy(&self.buf);
            // Frame terminator: "\n\n" or "\r\n\r\n".
            let end = match (text.find("\n\n"), text.find("\r\n\r\n")) {
                (Some(a), Some(b)) => Some(if a < b { (a, 2) } else { (b, 4) }),
                (Some(a), None) => Some((a, 2)),
                (None, Some(b)) => Some((b, 4)),
                (None, None) => None,
            };
            let Some((idx, sep)) = end else { break };
            let frame_text = text[..idx].to_owned();
            let consumed = idx + sep;
            drop(text);
            let _ = self.buf.split_to(consumed);
            if let Some(f) = parse_frame(&frame_text) {
                out.push(f);
            }
        }
        out
    }
}

fn parse_frame(text: &str) -> Option<SseFrame> {
    let mut event = None;
    let mut data: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with(':') {
            continue; // comment
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => event = Some(value.to_owned()),
            "data" => data.push(value),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    Some(SseFrame {
        event,
        data: data.join("\n"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_control_planes_shape_across_chunk_boundaries() {
        let mut p = SseParser::default();
        p.push(&Bytes::from_static(
            b": connected\n\nevent: message\ndata: {\"id\":\"sevt_1\",\"ty",
        ));
        assert!(
            p.frames().is_empty(),
            "comment alone is not a frame; data frame incomplete"
        );
        p.push(&Bytes::from_static(
            b"pe\":\"agent.message\"}\n\nevent: message\ndata: {\"id\":\"sevt_2\"}\n\n",
        ));
        let frames = p.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("message"));
        assert_eq!(
            frames[0].data,
            "{\"id\":\"sevt_1\",\"type\":\"agent.message\"}"
        );
        assert_eq!(frames[1].data, "{\"id\":\"sevt_2\"}");
        assert!(p.frames().is_empty());
    }

    #[test]
    fn multiline_data_and_crlf() {
        let mut p = SseParser::default();
        p.push(&Bytes::from_static(b"data: a\r\ndata: b\r\n\r\n"));
        let frames = p.frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "a\nb");
        assert_eq!(frames[0].event, None);
    }
}
