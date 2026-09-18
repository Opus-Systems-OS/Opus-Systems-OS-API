//! Per-key token bucket. `per_minute` is the sustained rate and also the
//! burst (a full bucket); 0 disables. In memory only — one API process, and
//! a limit that resets on restart is fine for what this protects against
//! (a runaway client, not an adversary).

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
        self.check_at(key_id, Instant::now())
    }

    fn check_at(&self, key_id: &str, now: Instant) -> Result<(), u32> {
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
            b.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - b.tokens) / per_sec;
            Err(wait.ceil().max(1.0) as u32)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_429_then_refill() {
        let rl = RateLimiter::new(60); // 1/s sustained, burst 60
        let t0 = Instant::now();
        for _ in 0..60 {
            assert!(rl.check_at("k", t0).is_ok());
        }
        assert_eq!(rl.check_at("k", t0), Err(1));
        // Another key is unaffected.
        assert!(rl.check_at("other", t0).is_ok());
        // 2.5 s later: two tokens back.
        let t1 = t0 + Duration::from_millis(2500);
        assert!(rl.check_at("k", t1).is_ok());
        assert!(rl.check_at("k", t1).is_ok());
        assert!(rl.check_at("k", t1).is_err());
    }

    #[test]
    fn zero_disables() {
        let rl = RateLimiter::disabled();
        for _ in 0..1000 {
            assert!(rl.check("k").is_ok());
        }
    }
}
