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

struct PriorityEntry {
    job: CrawlJob,
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
        self.job
            .priority
            .cmp(&other.job.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for PriorityEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct LocalFrontier {
    state: Mutex<FrontinerState>,
    dedup: Arc<dyn DedupFilter>,
    max_size: usize,
    priority_mode: PriorityMode,
    total_pushed: AtomicU64,
}

struct FrontinerState {
    heap: BinaryHeap<PriorityEntry>,
    seq: u64,
}

impl FrontinerState {
    fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            seq: 0,
        }
    }
}
