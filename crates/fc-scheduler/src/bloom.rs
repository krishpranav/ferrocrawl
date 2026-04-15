// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Probabilistic URL deduplication via a Bloom filter.
//!
//! ## Design
//!
//! A Bloom filter answers the question "have I seen this URL?" in O(1) time
//! with zero false negatives (no URL is ever incorrectly reported as unseen)
//! and a tunable false-positive rate (some unseen URLs are incorrectly
//! reported as seen — they simply get skipped, which is acceptable).
//!
//! At 100 million URLs and a 0.1% FPR, the filter requires ~180 MiB of RAM
//! and uses 7 hash functions. The implementation uses a `RwLock<Bloom<String>>`
//! with a double-checked-locking pattern: cheap shared reads for the hot path,
//! exclusive writes only on actual inserts.
//!
//! ## Thread safety
//!
//! `BloomDedupFilter` is `Send + Sync`. All state mutations go through
//! `std::sync::RwLock`, so concurrent calls to `check_and_insert` are safe.
//! The insertion count is tracked with an `AtomicU64` so fill-ratio queries
//! never need the write lock.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    RwLock,
};

use bloomfilter::Bloom;
use fc_core::traits::DedupFilter;

/// Bloom-filter-backed URL deduplication.
///
/// Construct via [`BloomDedupFilter::new`] with the expected number of unique
/// URLs and the desired false-positive rate.
///
/// ```rust
/// use fc_scheduler::BloomDedupFilter;
/// use fc_core::traits::DedupFilter;
///
/// let filter = BloomDedupFilter::new(10_000_000, 0.001);
/// assert!(filter.check_and_insert("https://example.com/page"));
/// assert!(!filter.check_and_insert("https://example.com/page")); // duplicate
/// ```
pub struct BloomDedupFilter {
    /// Inner Bloom filter, protected by a read-write lock.
    ///
    /// We use `Bloom<String>` rather than `Bloom<str>` because `bloomfilter`
    /// requires the type parameter to be `Sized`. The small allocation per
    /// check is negligible compared to the network round-trip it prevents.
    inner: RwLock<Bloom<String>>,

    /// Number of unique URLs inserted. Maintained with an atomic to allow
    /// lock-free fill-ratio queries.
    count: AtomicU64,

    /// Declared capacity (used to compute `fill_ratio`).
    capacity: usize,
}

impl BloomDedupFilter {
    /// Creates a new filter sized for `capacity` unique URLs at the given
    /// false-positive rate `fpr` (e.g. `0.001` for 0.1%).
    ///
    /// # Panics
    ///
    /// Panics if `capacity == 0` or `fpr` is not in the range `(0.0, 1.0)`.
    pub fn new(capacity: usize, fpr: f64) -> Self {
        assert!(capacity > 0, "bloom capacity must be positive");
        assert!(
            fpr > 0.0 && fpr < 1.0,
            "bloom false-positive rate must be in (0.0, 1.0)"
        );

        Self {
            inner: RwLock::new(Bloom::new_for_fp_rate(capacity, fpr)),
            count: AtomicU64::new(0),
            capacity,
        }
    }

    /// Returns the current insertion count. Cheaper than `len()` via the trait
    /// because it does not acquire any lock.
    #[inline]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl DedupFilter for BloomDedupFilter {
    /// Returns `true` if `url` was **not** already in the filter, and adds it.
    /// Returns `false` if it was already present.
    ///
    /// Uses a double-checked locking pattern to minimise contention:
    ///
    /// 1. Acquire a **shared** read lock and check membership (fast path —
    ///    for a high-traffic crawl the vast majority of URLs will already be
    ///    in the filter and this path handles them cheaply).
    /// 2. Only if the URL appears new: acquire the **exclusive** write lock,
    ///    check again (to handle the TOCTOU window between steps 1 and 2),
    ///    then insert.
    fn check_and_insert(&self, url: &str) -> bool {
        let owned = url.to_owned();

        // ── Fast path: read lock ──────────────────────────────────────────
        {
            let guard = self
                .inner
                .read()
                .expect("bloom filter read lock poisoned — this is a bug");
            if guard.check(&owned) {
                return false; // already seen
            }
        }

        // ── Slow path: write lock with double-check ───────────────────────
        let mut guard = self
            .inner
            .write()
            .expect("bloom filter write lock poisoned — this is a bug");

        if guard.check(&owned) {
            // Another thread inserted between our read-check and write-lock.
            return false;
        }

        guard.set(&owned);
        // Relaxed ordering is fine: the write lock provides the memory barrier.
        self.count.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn contains(&self, url: &str) -> bool {
        let guard = self
            .inner
            .read()
            .expect("bloom filter read lock poisoned — this is a bug");
        guard.check(&url.to_owned())
    }

    fn fill_ratio(&self) -> f64 {
        // Approximate: based on insertion count vs declared capacity.
        // The real filter fill ratio would require `number_of_bits_set / total_bits`
        // which the bloomfilter crate does not expose. This approximation is
        // sufficient for the Prometheus gauge.
        (self.count.load(Ordering::Relaxed) as f64 / self.capacity as f64).min(1.0)
    }

    fn len(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

// ── Safety ────────────────────────────────────────────────────────────────────
// `Bloom<String>` is `!Send` in older versions of the bloomfilter crate.
// We wrap it in RwLock which makes the wrapper Send + Sync as long as we
// never hand out references that outlive the lock guard. We don't.

unsafe impl Send for BloomDedupFilter {}
unsafe impl Sync for BloomDedupFilter {}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn insert_and_check() {
        let f = BloomDedupFilter::new(1_000, 0.001);
        assert!(f.check_and_insert("https://a.com/1"));
        assert!(f.check_and_insert("https://a.com/2"));
        assert!(!f.check_and_insert("https://a.com/1")); // duplicate
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn fill_ratio_increases_monotonically() {
        let f = BloomDedupFilter::new(100, 0.01);
        let r0 = f.fill_ratio();
        f.check_and_insert("https://a.com");
        let r1 = f.fill_ratio();
        assert!(r1 > r0);
    }

    #[test]
    fn concurrent_inserts_no_duplicates() {
        let filter = Arc::new(BloomDedupFilter::new(10_000, 0.001));
        let url = "https://concurrent.example.com/same";

        let handles: Vec<_> = (0..16)
            .map(|_| {
                let f = Arc::clone(&filter);
                let u = url.to_owned();
                thread::spawn(move || f.check_and_insert(&u))
            })
            .collect();

        let accepted: u64 = handles
            .into_iter()
            .map(|h| h.join().expect("thread panicked") as u64)
            .sum();

        // Exactly one thread must have inserted the URL.
        assert_eq!(
            accepted, 1,
            "expected exactly one acceptance, got {accepted}"
        );
        assert_eq!(filter.len(), 1);
    }
}
