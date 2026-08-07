//! Per-IP token-bucket rate limiting for the OPEN routes (webhooks, runner
//! callback, Slack interactions, inbox shell). Those endpoints verify their
//! own signatures/tokens, but verification costs CPU and (for webhooks) a
//! store lookup — this keeps an unauthenticated flood from turning that
//! into a denial of service. The bearer-authed product surface is not
//! limited; it is operator-facing and already gated.
//!
//! Deliberately dependency-free: a `HashMap<ip, bucket>` behind a mutex is
//! plenty for a single-tenant server, and keeps behavior deterministic
//! enough to unit-test the refill math.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Buckets beyond this count trigger a prune of stale entries (an IP that
/// has been idle long enough to refill completely carries no state worth
/// keeping).
const PRUNE_THRESHOLD: usize = 10_000;

pub struct RateLimiter {
    tokens_per_second: f64,
    burst: f64,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// `per_second = 0` disables limiting (returns `None`). Burst capacity
    /// is 3× the sustained rate, floor 30 — webhook bursts (GitHub delivers
    /// several events per push) must not trip it.
    pub fn from_rate(per_second: u32) -> Option<RateLimiter> {
        if per_second == 0 {
            return None;
        }
        Some(RateLimiter {
            tokens_per_second: f64::from(per_second),
            burst: f64::from((per_second * 3).max(30)),
            buckets: Mutex::new(HashMap::new()),
        })
    }

    /// Is this request within budget? `None` (peer address unknown — e.g.
    /// a test server without connect info) shares a single global bucket,
    /// which fails toward limiting rather than toward openness.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        self.allow_at(ip, Instant::now())
    }

    fn allow_at(&self, ip: Option<IpAddr>, now: Instant) -> bool {
        let key = ip.unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        if buckets.len() > PRUNE_THRESHOLD {
            let burst = self.burst;
            let rate = self.tokens_per_second;
            buckets.retain(|_, b| (now - b.last_refill).as_secs_f64() * rate < burst - b.tokens);
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            last_refill: now,
        });
        let elapsed = (now - bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.tokens_per_second).min(self.burst);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn zero_disables() {
        assert!(RateLimiter::from_rate(0).is_none());
    }

    #[test]
    fn burst_then_limited_then_refills() {
        let limiter = RateLimiter::from_rate(10).unwrap(); // burst 30
        let start = Instant::now();
        let ip = Some("203.0.113.9".parse().unwrap());
        for _ in 0..30 {
            assert!(limiter.allow_at(ip, start), "burst capacity admits 30");
        }
        assert!(
            !limiter.allow_at(ip, start),
            "31st in the same instant is limited"
        );
        // One second later, 10 tokens have refilled.
        let later = start + Duration::from_secs(1);
        for _ in 0..10 {
            assert!(limiter.allow_at(ip, later));
        }
        assert!(!limiter.allow_at(ip, later));
    }

    #[test]
    fn ips_are_limited_independently() {
        let limiter = RateLimiter::from_rate(10).unwrap();
        let start = Instant::now();
        let noisy = Some("203.0.113.9".parse().unwrap());
        for _ in 0..31 {
            limiter.allow_at(noisy, start);
        }
        assert!(!limiter.allow_at(noisy, start));
        let quiet = Some("198.51.100.7".parse().unwrap());
        assert!(limiter.allow_at(quiet, start), "another IP is unaffected");
    }

    #[test]
    fn unknown_peer_shares_one_bucket() {
        let limiter = RateLimiter::from_rate(10).unwrap();
        let start = Instant::now();
        for _ in 0..30 {
            assert!(limiter.allow_at(None, start));
        }
        assert!(!limiter.allow_at(None, start));
    }
}
