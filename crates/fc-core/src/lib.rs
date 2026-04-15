// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! # fc-core
//!
//! The foundational crate for ferrocrawl. Every other crate in the workspace
//! depends on this one and this one alone for shared types. It defines no I/O,
//! no networking, no async runtime — only pure data structures, traits, and
//! configuration types that the entire system builds on top of.
//!
//! ## Design principles
//!
//! - **Zero dependencies on runtime crates** — fc-core is intentionally thin.
//! - **`Send + Sync` everywhere** — all public types cross thread boundaries.
//! - **Exhaustive errors** — every failure mode is a named variant, not a string.
//! - **Builder patterns** — mutable setters are builder-style for ergonomic construction.

pub mod config;
pub mod error;
pub mod traits;
pub mod types;

// ── Convenience re-exports ───────────────────────────────────────────────────
// Downstream crates get the full public API with a single `use fc_core::prelude::*`.

pub mod prelude {
    pub use crate::config::{
        ApiConfig, ClickHouseConfig, CoreConfig, CrawlerConfig, DedupConfig, ExportConfig,
        PriorityMode, ProxyConfig, ProxyRotation, RateLimiterConfig, RedisConfig, SchedulerConfig,
        SinkType,
    };
    pub use crate::error::{CrawlError, Result};
    pub use crate::traits::{DedupFilter, Extractor, Fetcher, Scheduler, Sink};
    pub use crate::types::{
        CrawlJob, CrawlResult, CrawlStats, CrawlStatus, ExtractedRecord, FetchResponse, FieldValue,
        Priority, SkipReason,
    };
}
