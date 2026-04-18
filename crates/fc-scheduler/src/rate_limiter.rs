// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Per-domain async rate limiting via the `governor` token-bucket algorithm.
//!
//! ## Design
//!
//! Each domain gets its own [`governor::RateLimiter`] instance, created lazily
//! on first access and stored in a `DashMap` for lock-free concurrent reads.
//! Workers call [`DomainRateLimiter::until_ready`] before each fetch; it either
//! returns immediately (token available) or suspends the task until one is.
//!
//! Per-domain overrides in [`RateLimiterConfig::per_domain`] allow polite
//! crawling of high-value targets (`example.com` at 2 rps) while being more
//! aggressive with others.
//!
//! ## Why `governor`?
//!
//! `governor` implements the GCRA (Generic Cell Rate Algorithm), which is
//! equivalent to a token bucket but with better burst handling. It is
//! `no_std`-compatible, has zero unsafe code, and its `until_ready()` future
//! integrates cleanly with Tokio without spawning background tasks.

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use dashmap::DashMap;
use fc_core::config::RateLimiterConfig;
use governor::{
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use governor::clock::Clock;
use tracing::debug;

/// The concrete limiter type. One per domain, shared across worker tasks.
type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

/// Per-domain token-bucket rate limiter.
///
/// Thread-safe and cheaply clonable via the inner `Arc`. Construct from
/// [`RateLimiterConfig`] and share a single instance across all workers.
///
/// ```rust,no_run
/// # use fc_scheduler::DomainRateLimiter;
/// # use fc_core::config::RateLimiterConfig;
/// let limiter = DomainRateLimiter::from_config(&RateLimiterConfig::default());
/// // In an async context:
/// // limiter.until_ready("example.com").await;
/// ```
#[derive(Clone)]
pub struct DomainRateLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    /// Live limiter instances, keyed by bare hostname.
    limiters: DashMap<String, Arc<Limiter>>,
    /// Default RPS when no domain-specific override exists.
    default_rps: u32,
    /// Default burst allowance.
    default_burst: u32,
    /// Per-domain RPS overrides.
    per_domain: std::collections::HashMap<String, u32>,
}

impl DomainRateLimiter {
    /// Constructs a new rate limiter from the crawler configuration.
    pub fn from_config(cfg: &RateLimiterConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                limiters: DashMap::new(),
                default_rps: cfg.default_rps,
                default_burst: cfg.burst,
                per_domain: cfg.per_domain.clone(),
            }),
        }
    }

    /// Suspends the current task until the given domain's rate limiter grants
    /// a token. Returns immediately if one is available.
    ///
    /// The domain limiter is created lazily on first call for that domain,
    /// so there is no need to pre-register domains.
    pub async fn until_ready(&self, domain: &str) {
        let limiter = self.get_or_create(domain);
        limiter.until_ready().await;
        debug!(domain, "rate limiter: token granted");
    }

    /// Checks whether a token is available without consuming it.
    /// Useful for preemptive queue reordering.
    pub fn check(&self, domain: &str) -> bool {
        let limiter = self.get_or_create(domain);
        limiter.check().is_ok()
    }

    /// Returns the approximate wait duration before the next token is available
    /// for the given domain. Returns `Duration::ZERO` if a token is available now.
    pub fn wait_duration(&self, domain: &str) -> Duration {
        let limiter = self.get_or_create(domain);
        match limiter.check() {
            Ok(_) => Duration::ZERO,
            Err(not_until) => not_until.wait_time_from(DefaultClock::default().now()),
        }
    }

    /// Returns (or lazily creates) the limiter for `domain`.
    fn get_or_create(&self, domain: &str) -> Arc<Limiter> {
        // Fast path: domain already has a limiter.
        if let Some(existing) = self.inner.limiters.get(domain) {
            return Arc::clone(&*existing);
        }

        // Slow path: create a new limiter for this domain.
        let rps = self
            .inner
            .per_domain
            .get(domain)
            .copied()
            .unwrap_or(self.inner.default_rps);

        let limiter = Arc::new(Self::build_limiter(rps, self.inner.default_burst));

        // Use `entry` to avoid double-insertion if two tasks race here.
        self.inner
            .limiters
            .entry(domain.to_owned())
            .or_insert_with(|| Arc::clone(&limiter));

        limiter
    }

    fn build_limiter(rps: u32, burst: u32) -> Limiter {
        // Both rps and burst must be >= 1.
        let rps = NonZeroU32::new(rps.max(1)).expect("rps is always ≥ 1");
        let burst = NonZeroU32::new(burst.max(1)).expect("burst is always ≥ 1");

        let quota = Quota::per_second(rps).allow_burst(burst);
        RateLimiter::direct(quota)
    }

    /// Number of domains currently being tracked.
    pub fn domain_count(&self) -> usize {
        self.inner.limiters.len()
    }

    /// Removes the limiter for a given domain, resetting its token state.
    /// Useful in tests or when a domain's config changes at runtime.
    pub fn reset(&self, domain: &str) {
        self.inner.limiters.remove(domain);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use fc_core::config::RateLimiterConfig;

    fn cfg_with(default_rps: u32, burst: u32) -> RateLimiterConfig {
        RateLimiterConfig {
            default_rps,
            burst,
            per_domain: Default::default(),
        }
    }

    #[tokio::test]
    async fn token_available_immediately_at_start() {
        let rl = DomainRateLimiter::from_config(&cfg_with(10, 10));
        // With burst=10, first 10 calls should be instant.
        for _ in 0..10 {
            tokio::time::timeout(Duration::from_millis(5), rl.until_ready("example.com"))
                .await
                .expect("should not have blocked on burst capacity");
        }
    }

    #[tokio::test]
    async fn per_domain_override_applies() {
        let mut cfg = cfg_with(100, 100);
        cfg.per_domain.insert("slow.example.com".to_owned(), 1);

        let rl = DomainRateLimiter::from_config(&cfg);
        // The slow domain should have rps=1, the fast domain rps=100.
        assert!(rl.check("fast.example.com"));
        assert!(rl.check("slow.example.com"));
        // Drain the single token.
        rl.until_ready("slow.example.com").await;
        // Next token should not be immediate.
        let wait = rl.wait_duration("slow.example.com");
        assert!(
            wait > Duration::ZERO,
            "expected a wait after draining rps=1 limiter"
        );
    }

    #[test]
    fn lazily_creates_domain_limiters() {
        let rl = DomainRateLimiter::from_config(&cfg_with(10, 10));
        assert_eq!(rl.domain_count(), 0);
        rl.check("a.com");
        rl.check("b.com");
        assert_eq!(rl.domain_count(), 2);
    }
}
