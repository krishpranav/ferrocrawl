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

use dashmap::DashMap;
use fc_core::config::RateLimiterConfig;
use governor::{
    clock::DefaultClock,
    middleware::NoOpMiddleware,
    state::{InMemoryState, NotKeyed},
    Quota, RateLimiter,
};
use std::{num::NonZeroU32, sync::Arc, time::Duration};
use tracing::debug;

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock, NoOpMiddleware>;

#[derive(Clone)]
pub struct DomainRateLimiter {
    inner: Arc<Inner>,
}

struct Inner {
    limiters: DashMap<String, Arc<Limiter>>,
    default_rps: u32,
    default_burst: u32,
    per_domain: std::collections::HashMap<String, u32>,
}

impl DomainRateLimiter {
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

    pub async fn until_ready(&self, domain: &str) {
        let limiter = self.get_or_create(domain);
        limiter.until_ready().await;
        debug!(domain, "rate limiter: token granted");
    }

    pub fn domain_count(&self) -> usize {
        self.inner.limiters.len()
    }

    pub fn reset(&self, domain: &str) {
        self.inner.limiters.remove(domain);
    }
}

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
    }
}
