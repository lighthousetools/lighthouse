//! A small in-process rate limiter for sign-in and sign-up.
//!
//! It counts events per key, such as `ip:203.0.113.7` or `email:a@example.com`,
//! in fixed windows. Once a key reaches the limit, it stays blocked until its
//! window ends. Several server processes each keep their own counts, which is
//! enough to slow down guessing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Entries kept before stale ones are dropped.
const PRUNE_AT: usize = 10_000;

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    count: u32,
}

/// Counts events per key in fixed windows.
#[derive(Debug)]
pub struct RateLimiter {
    max: u32,
    window: Duration,
    entries: Mutex<HashMap<String, Window>>,
}

impl RateLimiter {
    /// Allows `max` events per key within `window`.
    pub fn new(max: u32, window: Duration) -> Self {
        Self {
            max,
            window,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Returns whether the key may act now.
    pub fn allowed(&self, key: &str) -> bool {
        self.allowed_at(key, Instant::now())
    }

    fn allowed_at(&self, key: &str, now: Instant) -> bool {
        let entries = self.entries.lock().expect("limiter lock");
        entries.get(key).is_none_or(|window| {
            now.duration_since(window.started) >= self.window || window.count < self.max
        })
    }

    /// Counts one event of the key.
    pub fn hit(&self, key: &str) {
        self.hit_at(key, Instant::now());
    }

    fn hit_at(&self, key: &str, now: Instant) {
        let mut entries = self.entries.lock().expect("limiter lock");
        if entries.len() >= PRUNE_AT {
            entries.retain(|_, window| now.duration_since(window.started) < self.window);
        }
        let window = entries.entry(key.to_owned()).or_insert(Window {
            started: now,
            count: 0,
        });
        if now.duration_since(window.started) >= self.window {
            *window = Window {
                started: now,
                count: 0,
            };
        }
        window.count += 1;
    }

    /// Forgets the events of a key, for example after a successful sign-in.
    pub fn reset(&self, key: &str) {
        self.entries.lock().expect("limiter lock").remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_the_limit_until_the_window_ends() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let start = Instant::now();
        assert!(limiter.allowed_at("ip:a", start));
        limiter.hit_at("ip:a", start);
        limiter.hit_at("ip:a", start);
        assert!(!limiter.allowed_at("ip:a", start + Duration::from_secs(1)));
        assert!(limiter.allowed_at("ip:b", start));
        let later = start + Duration::from_secs(61);
        assert!(limiter.allowed_at("ip:a", later));
        limiter.hit_at("ip:a", later);
        assert!(limiter.allowed_at("ip:a", later));
    }

    #[test]
    fn reset_forgets_a_key() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60));
        limiter.hit("email:x");
        assert!(!limiter.allowed("email:x"));
        limiter.reset("email:x");
        assert!(limiter.allowed("email:x"));
    }

    #[test]
    fn stale_entries_are_pruned() {
        let limiter = RateLimiter::new(1, Duration::from_secs(1));
        let start = Instant::now();
        for index in 0..PRUNE_AT {
            limiter.hit_at(&format!("k{index}"), start);
        }
        limiter.hit_at("fresh", start + Duration::from_secs(2));
        assert_eq!(limiter.entries.lock().unwrap().len(), 1);
    }
}
