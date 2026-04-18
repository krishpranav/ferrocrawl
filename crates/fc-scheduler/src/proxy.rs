// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Failure-aware proxy pool with configurable rotation strategies.
//!
//! ## Design
//!
//! The proxy pool maintains a list of [`ProxyEntry`] structs, each tracking
//! consecutive failure count and quarantine state. Proxy selection is lock-free
//! for the round-robin cursor (`AtomicUsize`) and uses fine-grained atomics for
//! failure tracking, so the hot path (selecting a proxy per request) never
//! blocks.
//!
//! When a proxy accumulates `failure_threshold` consecutive failures, it is
//! quarantined for `quarantine_duration`. After the quarantine period elapses,
//! the proxy is automatically re-enabled on the next selection attempt.

use std::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use fc_core::config::{ProxyConfig, ProxyRotation};
use tracing::{info, warn};

// ── ProxyEntry ────────────────────────────────────────────────────────────────

/// Internal state for a single proxy in the pool.
struct ProxyEntry {
    /// The proxy URL (e.g. `http://proxy:8080` or `socks5://proxy:1080`).
    url: String,

    /// Consecutive failure count. Reset to 0 on any success.
    failures: AtomicU32,

    /// Whether this proxy is currently quarantined.
    quarantined: AtomicBool,

    /// When the quarantine period began. `None` if not quarantined.
    /// Access is racy (no lock), but the worst outcome is a brief double-ban
    /// or premature unban — both are acceptable in a proxy pool context.
    quarantined_at: std::sync::Mutex<Option<Instant>>,
}

impl ProxyEntry {
    fn new(url: String) -> Self {
        Self {
            url,
            failures: AtomicU32::new(0),
            quarantined: AtomicBool::new(false),
            quarantined_at: std::sync::Mutex::new(None),
        }
    }

    /// Returns `true` if this proxy is currently available (not quarantined,
    /// or quarantine period has elapsed).
    fn is_available(&self, quarantine_duration: Duration) -> bool {
        if !self.quarantined.load(Ordering::Relaxed) {
            return true;
        }

        // Check whether the quarantine has expired.
        let guard = self.quarantined_at.lock().unwrap();
        if let Some(since) = *guard {
            if since.elapsed() >= quarantine_duration {
                drop(guard);
                self.quarantined.store(false, Ordering::Relaxed);
                self.failures.store(0, Ordering::Relaxed);
                info!(proxy = %self.url, "proxy quarantine lifted; re-enabling");
                return true;
            }
        }
        false
    }

    fn record_success(&self) {
        self.failures.store(0, Ordering::Relaxed);
    }

    fn record_failure(&self, threshold: u32, quarantine_duration: Duration) {
        let count = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= threshold && !self.quarantined.load(Ordering::Acquire) {
            warn!(
                proxy = %self.url,
                failures = count,
                "proxy exceeded failure threshold; quarantining for {:?}",
                quarantine_duration
            );
            *self.quarantined_at.lock().unwrap() = Some(Instant::now());
            self.quarantined.store(true, Ordering::Release);
        }
    }
}

// ── ProxyPool ─────────────────────────────────────────────────────────────────

/// A pool of HTTP/SOCKS5 proxies with failure tracking and rotation.
///
/// When no proxies are configured or all proxies are quarantined, `next_proxy`
/// returns `None` and the fetcher falls back to a direct connection.
pub struct ProxyPool {
    entries: Vec<ProxyEntry>,
    rotation: ProxyRotation,
    cursor: AtomicUsize,
    failure_threshold: u32,
    quarantine_duration: Duration,
}

impl ProxyPool {
    /// Creates a new pool from the crawler's proxy configuration.
    /// Returns `None` if the proxy feature is disabled or no proxies are listed.
    pub fn from_config(cfg: &ProxyConfig) -> Option<Self> {
        if !cfg.enabled || cfg.proxies.is_empty() {
            return None;
        }

        let entries = cfg
            .proxies
            .iter()
            .map(|url| ProxyEntry::new(url.clone()))
            .collect();

        Some(Self {
            entries,
            rotation: cfg.rotation.clone(),
            cursor: AtomicUsize::new(0),
            failure_threshold: cfg.failure_threshold,
            quarantine_duration: Duration::from_secs(cfg.quarantine_secs),
        })
    }

    /// Returns the URL of the next available proxy, or `None` if all proxies
    /// are quarantined.
    pub fn next_proxy(&self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        match &self.rotation {
            ProxyRotation::RoundRobin => self.next_round_robin(),
            ProxyRotation::Random => self.next_random(),
            ProxyRotation::StickyDomain => {
                // StickyDomain requires a domain key; fall back to round-robin
                // when called without one.
                self.next_round_robin()
            }
        }
    }

    /// Returns the proxy to use for a specific domain (sticky mode).
    ///
    /// The domain is hashed to a consistent index, so the same domain always
    /// maps to the same proxy as long as the pool size doesn't change.
    pub fn proxy_for_domain(&self, domain: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        // Simple but stable: FNV-1a hash of the domain string.
        let idx = fnv1a(domain.as_bytes()) as usize % self.entries.len();
        self.find_available_from(idx)
    }

    /// Records a successful request through the given proxy URL.
    pub fn record_success(&self, proxy_url: &str) {
        if let Some(entry) = self.entries.iter().find(|e| e.url == proxy_url) {
            entry.record_success();
        }
    }

    /// Records a failed request through the given proxy URL.
    ///
    /// Once `failure_threshold` consecutive failures are recorded, the proxy
    /// is quarantined for `quarantine_duration`.
    pub fn record_failure(&self, proxy_url: &str) {
        if let Some(entry) = self.entries.iter().find(|e| e.url == proxy_url) {
            entry.record_failure(self.failure_threshold, self.quarantine_duration);
        }
    }

    /// Total proxies in the pool (including quarantined ones).
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// Number of proxies currently available (not quarantined).
    pub fn available(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_available(self.quarantine_duration))
            .count()
    }

    fn next_round_robin(&self) -> Option<&str> {
        let n = self.entries.len();
        // Try each proxy once before giving up.
        for i in 0..n {
            let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
            if self.entries[idx].is_available(self.quarantine_duration) {
                return Some(&self.entries[idx].url);
            }
            let _ = i; // suppress unused warning
        }
        None // all quarantined
    }

    fn next_random(&self) -> Option<&str> {
        // Xorshift32 seeded from the cursor — no extra deps, no locking.
        let seed = self.cursor.fetch_add(1, Ordering::Relaxed) as u32;
        let idx = xorshift32(seed) as usize % self.entries.len();
        self.find_available_from(idx)
    }

    /// Starting at `start`, wraps around to find the first available proxy.
    fn find_available_from(&self, start: usize) -> Option<&str> {
        let n = self.entries.len();
        for offset in 0..n {
            let idx = (start + offset) % n;
            if self.entries[idx].is_available(self.quarantine_duration) {
                return Some(&self.entries[idx].url);
            }
        }
        None
    }
}

// ── Utility functions ─────────────────────────────────────────────────────────

/// FNV-1a hash — fast, no-std, good enough for proxy index derivation.
fn fnv1a(data: &[u8]) -> u32 {
    const OFFSET: u32 = 2_166_136_261;
    const PRIME: u32 = 16_777_619;
    data.iter().fold(OFFSET, |hash, &byte| {
        (hash ^ byte as u32).wrapping_mul(PRIME)
    })
}

/// One step of xorshift32, used as a cheap pseudo-random index.
fn xorshift32(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use fc_core::config::{ProxyConfig, ProxyRotation};

    fn cfg(proxies: &[&str]) -> ProxyConfig {
        ProxyConfig {
            enabled: true,
            rotation: ProxyRotation::RoundRobin,
            proxies: proxies.iter().map(|s| s.to_string()).collect(),
            failure_threshold: 3,
            quarantine_secs: 60,
        }
    }

    #[test]
    fn round_robin_cycles() {
        let pool = ProxyPool::from_config(&cfg(&["http://a:80", "http://b:80", "http://c:80"]))
            .unwrap();
        let first = pool.next_proxy().unwrap().to_owned();
        let second = pool.next_proxy().unwrap().to_owned();
        let third = pool.next_proxy().unwrap().to_owned();
        let fourth = pool.next_proxy().unwrap().to_owned();
        assert_ne!(first, second);
        assert_ne!(second, third);
        assert_eq!(first, fourth); // wrapped back around
    }

    #[test]
    fn quarantined_proxy_is_skipped() {
        let pool = ProxyPool::from_config(&cfg(&["http://bad:80", "http://good:80"])).unwrap();

        // Drive bad proxy to its failure threshold.
        for _ in 0..3 {
            pool.record_failure("http://bad:80");
        }

        // Every selection should now return the good proxy.
        for _ in 0..6 {
            let selected = pool.next_proxy().unwrap();
            assert_eq!(selected, "http://good:80");
        }
    }

    #[test]
    fn none_when_proxy_disabled() {
        let cfg = ProxyConfig { enabled: false, ..Default::default() };
        assert!(ProxyPool::from_config(&cfg).is_none());
    }

    #[test]
    fn sticky_domain_is_consistent() {
        let pool = ProxyPool::from_config(&cfg(&["http://a:80", "http://b:80", "http://c:80"]))
            .unwrap();
        let first = pool.proxy_for_domain("stripe.com").map(str::to_owned);
        let second = pool.proxy_for_domain("stripe.com").map(str::to_owned);
        assert_eq!(first, second);
    }
}