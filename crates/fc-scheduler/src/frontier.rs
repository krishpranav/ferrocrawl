// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! In-process priority-queue frontier for single-node crawling.
//!
//! ## Data structure
//!
//! The frontier is a `BinaryHeap<PriorityEntry>` behind a `tokio::sync::Mutex`.
//! Rust's `BinaryHeap` is a max-heap: the element with the greatest `Ord`
//! value is popped first. `PriorityEntry`'s `Ord` implementation ensures:
//!
//! 1. Higher `Priority` values are dequeued first.
//! 2. Within the same priority, lower sequence numbers (earlier arrivals)
//!    are dequeued first — FIFO semantics within a tier.
//!
//! ## Deduplication
//!
//! `LocalFrontier` owns a `Arc<dyn DedupFilter>` and calls `check_and_insert`
//! on every `push`. If the URL has already been seen, the job is silently
//! discarded and `push` returns `Ok(false)`.
//!
//! ## Priority modes
//!
//! The scheduler assigns an effective `Priority` based on the configured
//! [`PriorityMode`][fc_core::config::PriorityMode]:
//!
//! - `Breadth`: priority = `255 - depth` (shallower = higher priority).
//! - `Depth`:   priority = `depth.min(255)` (deeper = higher priority).
//! - `Score`:   priority taken from `job.priority` unchanged.

use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc,
    },
};

use async_trait::async_trait;
use fc_core::{
    config::PriorityMode,
    error::{CrawlError, Result},
    traits::{DedupFilter, Scheduler},
    types::{CrawlJob, Priority},
};
use tokio::sync::Mutex;
use tracing::{debug, warn};

// ── PriorityEntry ─────────────────────────────────────────────────────────────

/// Wrapper that gives `CrawlJob` a total ordering for the `BinaryHeap`.
struct PriorityEntry {
    job: CrawlJob,
    /// Monotonically increasing insertion sequence number.
    /// Used to break ties within the same priority level (FIFO).
    seq: u64,
}

impl PartialEq for PriorityEntry {
    fn eq(&self, other: &Self) -> bool {
        self.job.priority == other.job.priority && self.seq == other.seq
    }
}

impl Eq for PriorityEntry {}

impl Ord for PriorityEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher Priority → greater → popped first (max-heap).
        self.job
            .priority
            .cmp(&other.job.priority)
            // Within the same priority: lower seq (earlier insertion) → greater.
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for PriorityEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ── LocalFrontier ─────────────────────────────────────────────────────────────

/// An in-process, priority-ordered frontier queue.
///
/// Suitable for single-node crawls of any size that fits the Bloom filter in
/// memory. For distributed crawls across multiple machines, replace this with
/// the Redis-backed frontier in `fc-worker`.
pub struct LocalFrontier {
    /// The heap and the sequence counter, co-located under one lock to keep
    /// push/pop atomic with respect to each other.
    state: Mutex<FrontierState>,

    /// Shared deduplication filter.
    dedup: Arc<dyn DedupFilter>,

    /// Maximum number of pending jobs. Push fails gracefully when full.
    max_size: usize,

    /// Priority assignment strategy.
    priority_mode: PriorityMode,

    /// Total URLs ever accepted (not deduplicated). For metrics.
    total_pushed: AtomicU64,
}

struct FrontierState {
    heap: BinaryHeap<PriorityEntry>,
    seq: u64,
}

impl FrontierState {
    fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            seq: 0,
        }
    }

    fn push(&mut self, job: CrawlJob) {
        self.seq += 1;
        self.heap.push(PriorityEntry { job, seq: self.seq });
    }

    fn pop(&mut self) -> Option<CrawlJob> {
        self.heap.pop().map(|e| e.job)
    }

    fn len(&self) -> usize {
        self.heap.len()
    }
}

impl LocalFrontier {
    /// Creates a new frontier with the given configuration.
    ///
    /// # Arguments
    ///
    /// * `dedup`         — URL deduplication filter (typically a `BloomDedupFilter`).
    /// * `max_size`      — Maximum number of pending jobs. See `SchedulerConfig::max_queue_size`.
    /// * `priority_mode` — How to assign priority to incoming jobs.
    pub fn new(dedup: Arc<dyn DedupFilter>, max_size: usize, priority_mode: PriorityMode) -> Self {
        Self {
            state: Mutex::new(FrontierState::new()),
            dedup,
            max_size,
            priority_mode,
            total_pushed: AtomicU64::new(0),
        }
    }

    /// Total number of URLs ever accepted by the frontier (not deduplicated,
    /// regardless of whether they were subsequently popped).
    pub fn total_pushed(&self) -> u64 {
        self.total_pushed.load(AtomicOrdering::Relaxed)
    }

    /// Current depth of the queue. Acquires the lock briefly.
    pub async fn current_len(&self) -> usize {
        self.state.lock().await.len()
    }

    /// Computes the effective priority for a job under the current mode.
    fn effective_priority(job: &CrawlJob, mode: &PriorityMode) -> Priority {
        match mode {
            PriorityMode::Breadth => {
                // Shallower pages get higher priority.
                // depth=0 (seeds) → Priority(255), depth=255+ → Priority(0).
                Priority(255u8.saturating_sub(job.depth.min(255) as u8))
            }
            PriorityMode::Depth => {
                // Deeper pages get higher priority.
                Priority(job.depth.min(255) as u8)
            }
            PriorityMode::Score => {
                // Caller is responsible for setting job.priority before pushing.
                job.priority
            }
        }
    }
}

#[async_trait]
impl Scheduler for LocalFrontier {
    /// Attempts to enqueue `job`.
    ///
    /// The URL is normalised (fragment stripped, query sorted) before dedup so
    /// that `https://example.com/page?b=2&a=1` and `https://example.com/page?a=1&b=2`
    /// are treated as the same URL.
    ///
    /// Returns:
    /// - `Ok(true)` — job accepted and queued.
    /// - `Ok(false)` — URL already seen; job silently discarded.
    /// - `Err(CrawlError::Scheduler)` — frontier is at capacity.
    async fn push(&self, mut job: CrawlJob) -> Result<bool> {
        // Normalise: strip fragment, which never affects server-side content.
        let mut url = job.url.clone();
        url.set_fragment(None);
        job.url = url;

        let url_str = job.url.as_str();

        // Deduplication check-and-insert (atomic).
        if !self.dedup.check_and_insert(url_str) {
            debug!(url = url_str, "dedup: already seen, skipping");
            return Ok(false);
        }

        // Assign effective priority based on mode.
        job.priority = Self::effective_priority(&job, &self.priority_mode);

        let mut state = self.state.lock().await;

        if state.len() >= self.max_size {
            warn!(
                url = url_str,
                queue_len = state.len(),
                max = self.max_size,
                "frontier at capacity; dropping URL"
            );
            // The URL was inserted into the dedup filter above, so we won't
            // retry it. This is intentional: the frontier being full means we
            // already have more work than we can handle.
            return Err(CrawlError::Scheduler(format!(
                "frontier at capacity ({} / {})",
                state.len(),
                self.max_size
            )));
        }

        state.push(job);
        self.total_pushed.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(true)
    }

    async fn pop(&self) -> Result<Option<CrawlJob>> {
        let mut state = self.state.lock().await;
        Ok(state.pop())
    }

    async fn len(&self) -> Result<u64> {
        Ok(self.state.lock().await.len() as u64)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bloom::BloomDedupFilter;
    use fc_core::config::PriorityMode;
    use url::Url;

    fn make_frontier(mode: PriorityMode) -> LocalFrontier {
        let dedup = Arc::new(BloomDedupFilter::new(10_000, 0.001));
        LocalFrontier::new(dedup, 1_000, mode)
    }

    fn job(url: &str, depth: u32) -> CrawlJob {
        CrawlJob::new(Url::parse(url).unwrap(), depth)
    }

    #[tokio::test]
    async fn dedup_rejects_duplicate_urls() {
        let f = make_frontier(PriorityMode::Breadth);
        assert!(f.push(job("https://example.com/a", 0)).await.unwrap());
        assert!(!f.push(job("https://example.com/a", 0)).await.unwrap());
        assert_eq!(f.len().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn breadth_mode_shallow_first() {
        let f = make_frontier(PriorityMode::Breadth);
        f.push(job("https://example.com/deep", 5)).await.unwrap();
        f.push(job("https://example.com/shallow", 1)).await.unwrap();
        f.push(job("https://example.com/seed", 0)).await.unwrap();

        // Seed (depth 0) should come out first.
        let first = f.pop().await.unwrap().unwrap();
        assert_eq!(first.url.as_str(), "https://example.com/seed");

        let second = f.pop().await.unwrap().unwrap();
        assert_eq!(second.url.as_str(), "https://example.com/shallow");
    }

    #[tokio::test]
    async fn empty_pop_returns_none() {
        let f = make_frontier(PriorityMode::Breadth);
        assert!(f.pop().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn fragment_normalisation() {
        let f = make_frontier(PriorityMode::Breadth);
        // With and without fragment are the same URL.
        assert!(f.push(job("https://example.com/page", 0)).await.unwrap());
        assert!(!f
            .push(job("https://example.com/page#section", 0))
            .await
            .unwrap());
    }
}
