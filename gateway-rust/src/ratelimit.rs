//! Minimal per-key fixed-window rate limiter. Keyed by verified address (only reachable
//! after signature + ownership verification), which is exactly the per-wallet limiting that
//! is impossible at a CDN edge (the edge can't resolve the wallet).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A single fixed-window bucket for one key.
struct Window {
    /// When this 60-second window started.
    start: Instant,
    /// Number of requests accepted in this window so far.
    count: u32,
}

/// Per-address, fixed 60-second window rate limiter.
pub struct RateLimiter {
    /// Maximum allowed requests per address per minute. 0 disables limiting.
    max_per_min: u32,
    /// Per-address window state, protected by a mutex.
    inner: Mutex<HashMap<String, Window>>,
}

impl RateLimiter {
    /// Create a new limiter with the given per-address, per-minute budget.
    pub fn new(max_per_min: u32) -> Self {
        Self {
            max_per_min,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the request is allowed (and records it). `max_per_min == 0` disables
    /// limiting.
    pub fn check(&self, key: &str) -> bool {
        if self.max_per_min == 0 {
            return true;
        }
        let now = Instant::now();
        let mut m = self.inner.lock().unwrap();
        let w = m.entry(key.to_string()).or_insert(Window {
            start: now,
            count: 0,
        });
        if now.duration_since(w.start) >= Duration::from_secs(60) {
            w.start = now;
            w.count = 0;
        }
        if w.count >= self.max_per_min {
            false
        } else {
            w.count += 1;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_limit_then_denies() {
        let rl = RateLimiter::new(3);
        assert!(rl.check("a"));
        assert!(rl.check("a"));
        assert!(rl.check("a"));
        assert!(!rl.check("a")); // 4th within the window
        assert!(rl.check("b")); // independent key
    }

    #[test]
    fn zero_disables() {
        let rl = RateLimiter::new(0);
        for _ in 0..100 {
            assert!(rl.check("x"));
        }
    }
}
