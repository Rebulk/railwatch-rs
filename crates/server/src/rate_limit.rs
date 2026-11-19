//! The two ingest rate limits, in memory instead of Solid Cache:
//! - IngestRequestRateLimit: per client IP, 2,400 requests a minute on any
//!   /ingest path, checked before authentication. Only requests that do not
//!   authenticate count toward it (GA review 2026-11-19 T4): tenants behind
//!   one egress address must not use up each other's;
//! - BaseController `rate_limit`: per environment, 600 a minute, after
//!   authentication.
//!
//! Both are Rails' shape: `cache.increment(key, 1, expires_in: 1.minute)`,
//! i.e. a counter that starts at the first request and expires a minute
//! later, and a request is refused once the count exceeds the limit.
//! A limit of 0 disables it.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const WINDOW: Duration = Duration::from_secs(60);

pub struct FixedWindow<K> {
    limit: u64,
    window: Duration,
    counters: Mutex<HashMap<K, (Instant, u64)>>,
}

impl<K: Hash + Eq + Clone> FixedWindow<K> {
    pub fn new(limit: u64) -> Self {
        Self::with_window(limit, WINDOW)
    }

    /// Rails' `rate_limit to:, within:` for the credential endpoints.
    pub fn with_window(limit: u64, window: Duration) -> Self {
        FixedWindow { limit, window, counters: Mutex::new(HashMap::new()) }
    }

    /// Counts the request; true when it must be refused.
    pub fn exceeded(&self, key: &K) -> bool {
        self.exceeded_at(key, Instant::now())
    }

    /// Would the next request for `key` be refused? Counts nothing: for a
    /// limit that only failed requests count toward (`exceeded` them).
    pub fn full(&self, key: &K) -> bool {
        let now = Instant::now();
        self.limit != 0 && self.counters.lock().unwrap().get(key).is_some_and(|(start, n)| now.duration_since(*start) < self.window && *n >= self.limit)
    }

    pub fn exceeded_at(&self, key: &K, now: Instant) -> bool {
        if self.limit == 0 {
            return false;
        }
        let mut counters = self.counters.lock().unwrap();
        if counters.len() > 100_000 {
            counters.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = counters.entry(key.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 > self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_past_the_limit_until_the_window_expires() {
        let rl = FixedWindow::new(3);
        let t0 = Instant::now();
        assert!(!rl.exceeded_at(&"a", t0));
        assert!(!rl.exceeded_at(&"a", t0));
        assert!(!rl.exceeded_at(&"a", t0));
        assert!(rl.exceeded_at(&"a", t0 + Duration::from_secs(59)));
        assert!(!rl.exceeded_at(&"b", t0), "keys are independent");
        assert!(!rl.exceeded_at(&"a", t0 + Duration::from_secs(60)), "a new window starts");
        let full = FixedWindow::new(2);
        assert!(!full.full(&"k"));
        full.exceeded(&"k");
        assert!(!full.full(&"k"), "one left");
        full.exceeded(&"k");
        assert!(full.full(&"k") && full.full(&"k"), "full, and asking counts nothing");
        let off = FixedWindow::new(0);
        assert!((0..10_000).all(|_| !off.exceeded(&"a")));
    }
}
