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

use bloomfilter::Bloom;
use fc_core::traits::DedupFilter;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    RwLock,
};
