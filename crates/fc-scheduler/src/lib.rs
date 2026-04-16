// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! # fc-scheduler
//!
//! The scheduling layer: everything between "here is a URL" and "here is the
//! next job to give to a worker". Concretely:
//!
//! - [`bloom`] — probabilistic URL deduplication via a Bloom filter.
//! - [`frontier`] — an in-process priority-queue frontier backed by a
//!   `BinaryHeap`, with integrated deduplication.
//! - [`rate_limiter`] — per-domain token-bucket rate limiting via `governor`.
//! - [`proxy`] — failure-aware round-robin proxy pool.
//!
//! ## Choosing a scheduler implementation
//!
//! For single-node deployments use [`frontier::LocalFrontier`]. For distributed
//! deployments, the `fc-worker` crate (phase 2) provides a Redis-backed
//! implementation of the same [`Scheduler`][fc_core::traits::Scheduler] trait.

pub mod bloom;
pub mod frontier;

pub use bloom::BloomDedupFilter;
