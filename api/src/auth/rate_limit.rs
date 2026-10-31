//! Token buckets, keyed by a key id or a peer address. `per_minute` is the
//! sustained rate and also the burst (a full bucket); 0 disables. In memory
//! only — one API process, and a limit that resets on restart is fine for
//! what this protects against (a runaway client, a guesser).

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Evict idle buckets once the map grows past this many keys.
const SWEEP_AT: usize = 10_000;

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        RateLimiter {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn disabled() -> Self {
        Self::new(0)
    }

    /// `Ok(())` to proceed, `Err(retry_after_secs)` when the key is over.
    pub fn check(&self, key_id: &str) -> Result<(), u32> {
        self.check_at(key_id, Instant::now(), true)
    }

    /// Like `check`, without spending a token: for a bucket charged only on
    /// failure, asked before the attempt.
    pub fn blocked(&self, key_id: &str) -> Result<(), u32> {
        self.check_at(key_id, Instant::now(), false)
    }

    fn check_at(&self, key_id: &str, now: Instant, spend: bool) -> Result<(), u32> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let capacity = f64::from(self.per_minute);
        let per_sec = capacity / 60.0;
        let mut buckets = self.buckets.lock().expect("rate limiter mutex poisoned");
        if buckets.len() > SWEEP_AT {
            buckets.retain(|_, b| now.duration_since(b.last) < Duration::from_secs(120));
        }
        let b = buckets.entry(key_id.to_owned()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * per_sec).min(capacity);
        b.last = now;
        if b.tokens >= 1.0 {
            if spend {
                b.tokens -= 1.0;
            }
            Ok(())
        } else {
            let wait = (1.0 - b.tokens) / per_sec;
            Err(wait.ceil().max(1.0) as u32)
        }
    }
}

/// The caller's address, for the per-address buckets: the first
/// `X-Forwarded-For` hop (Caddy replaces the header with the client it
/// saw), else "unknown" — every direct caller shares one bucket, which only
/// happens in tests and on the compose network.
pub fn peer(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_429_then_refill() {
        let rl = RateLimiter::new(60); // 1/s sustained, burst 60
        let t0 = Instant::now();
        for _ in 0..60 {
            assert!(rl.check_at("k", t0, true).is_ok());
        }
        assert_eq!(rl.check_at("k", t0, true), Err(1));
        // Another key is unaffected.
        assert!(rl.check_at("other", t0, true).is_ok());
        // 2.5 s later: two tokens back.
        let t1 = t0 + Duration::from_millis(2500);
        assert!(rl.check_at("k", t1, true).is_ok());
        assert!(rl.check_at("k", t1, true).is_ok());
        assert!(rl.check_at("k", t1, true).is_err());
    }

    #[test]
    fn blocked_peeks_without_spending() {
        let rl = RateLimiter::new(2);
        let t0 = Instant::now();
        for _ in 0..5 {
            assert!(rl.check_at("p", t0, false).is_ok());
        }
        assert!(rl.check_at("p", t0, true).is_ok());
        assert!(rl.check_at("p", t0, true).is_ok());
        assert!(
            rl.check_at("p", t0, false).is_err(),
            "empty after two failures"
        );
    }

    #[test]
    fn peer_is_the_first_forwarded_hop() {
        let mut h = HeaderMap::new();
        assert_eq!(peer(&h), "unknown");
        h.insert("x-forwarded-for", "203.0.113.9, 10.0.0.2".parse().unwrap());
        assert_eq!(peer(&h), "203.0.113.9");
    }

    #[test]
    fn zero_disables() {
        let rl = RateLimiter::disabled();
        for _ in 0..1000 {
            assert!(rl.check("k").is_ok());
        }
    }
}
