// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Core trait definitions for every pluggable component in the pipeline.
//!
//! These traits are the seams between crates. They are intentionally thin —
//! no base implementations, no default behaviour — so that each implementation
//! crate (fc-engine, fc-scheduler, fc-export) can make its own choices about
//! concurrency model, batching, and resource management.
//!
//! All traits are `dyn`-safe where possible, allowing runtime polymorphism
//! for plugin-style architectures and testing with mock implementations.

use async_trait::async_trait;

use crate::{
    error::Result,
    types::{CrawlJob, ExtractedRecord, FetchResponse},
};

// ── Fetcher ───────────────────────────────────────────────────────────────────

/// Fetches a URL and returns the raw HTTP response.
///
/// The engine holds a single `Arc<dyn Fetcher>` shared across all worker tasks.
/// Implementations must be `Send + Sync` and internally manage their connection
/// pool, proxy rotation, and TLS configuration.
#[async_trait]
pub trait Fetcher: Send + Sync + 'static {
    /// Performs the HTTP request described by `job` and returns the response.
    ///
    /// Implementations are responsible for:
    /// - Respecting `CrawlerConfig::request_timeout`
    /// - Following redirects up to `CrawlerConfig::max_redirects`
    /// - Decompressing brotli/gzip bodies
    /// - Setting the configured `User-Agent`
    async fn fetch(&self, job: &CrawlJob) -> Result<FetchResponse>;

    /// Returns `true` if this fetcher can handle the given URL scheme.
    ///
    /// The engine calls this before dispatching to allow multi-scheme setups
    /// (e.g. `http`/`https` for the web fetcher, `wss` for the Solana module).
    fn supports_scheme(&self, scheme: &str) -> bool {
        matches!(scheme, "http" | "https")
    }

    /// Human-readable name for metrics and log lines.
    fn name(&self) -> &'static str;
}

// ── Extractor ─────────────────────────────────────────────────────────────────

/// Parses a [`FetchResponse`] and extracts a structured [`ExtractedRecord`].
///
/// Multiple extractors can be active simultaneously. The engine calls `matches`
/// on each registered extractor and runs all that return `true`.
#[async_trait]
pub trait Extractor: Send + Sync + 'static {
    /// Returns `true` if this extractor should run for the given response.
    ///
    /// Implementations typically check the URL against their configured
    /// `url_pattern` and optionally the `Content-Type` header.
    fn matches(&self, response: &FetchResponse) -> bool;

    /// Extracts structured data from the response.
    ///
    /// Must not mutate any shared state — this is called concurrently from
    /// many worker tasks.
    async fn extract(&self, response: &FetchResponse) -> Result<ExtractedRecord>;

    /// The schema name this extractor is bound to. Used in log lines and
    /// as the `schema` field of every record it produces.
    fn schema_name(&self) -> &str;
}

// ── Sink ──────────────────────────────────────────────────────────────────────

/// Persists [`ExtractedRecord`]s to an underlying storage backend.
///
/// Implementations may buffer writes internally (e.g. Parquet batches) and
/// flush them in bulk. The engine calls `flush` periodically and `close`
/// at shutdown.
#[async_trait]
pub trait Sink: Send + Sync + 'static {
    /// Accepts a single record. May buffer internally.
    async fn write(&self, record: ExtractedRecord) -> Result<()>;

    /// Flushes any buffered data to the underlying storage.
    ///
    /// The engine calls this on a configurable interval (e.g. every 30s)
    /// to bound the data loss window in case of an unclean shutdown.
    async fn flush(&self) -> Result<()>;

    /// Human-readable name for metrics and log lines.
    fn name(&self) -> &'static str;
}

// ── DedupFilter ───────────────────────────────────────────────────────────────

/// Tracks which URLs have already been seen, to prevent re-crawling.
///
/// Implementations may be probabilistic (Bloom filter) or exact (hash set,
/// Redis `SADD`). The `check_and_insert` operation must be atomic — in a
/// concurrent context, two threads calling it with the same URL simultaneously
/// must have exactly one return `true`.
pub trait DedupFilter: Send + Sync + 'static {
    /// Returns `true` if the URL has **not** been seen before, and marks it
    /// as seen. Returns `false` if it was already present.
    ///
    /// This is the only method callers should use for frontier decisions.
    fn check_and_insert(&self, url: &str) -> bool;

    /// Read-only membership test. Does not insert.
    fn contains(&self, url: &str) -> bool;

    /// Estimated fill ratio (0.0 = empty, 1.0 = at declared capacity).
    ///
    /// For Bloom filters this is an approximation based on insertion count.
    /// Exposed as the `fc_bloom_fill_ratio` Prometheus gauge.
    fn fill_ratio(&self) -> f64;

    /// Number of unique URLs inserted so far.
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ── Scheduler ─────────────────────────────────────────────────────────────────

/// Manages the crawl frontier: a priority queue of pending [`CrawlJob`]s.
///
/// The scheduler is the single point of truth for "what to crawl next". It
/// integrates deduplication, priority ordering, and capacity enforcement.
/// Workers call `pop` in a loop; the coordinator feeds new URLs via `push`.
#[async_trait]
pub trait Scheduler: Send + Sync + 'static {
    /// Enqueues a job.
    ///
    /// Returns `Ok(true)` if the job was accepted, `Ok(false)` if the URL
    /// was already in the seen-set and was deduplicated away. Returns `Err`
    /// only for infrastructure failures (e.g. Redis unreachable).
    async fn push(&self, job: CrawlJob) -> Result<bool>;

    /// Dequeues the highest-priority pending job.
    ///
    /// Returns `None` if the frontier is currently empty. Workers should
    /// back off briefly (e.g. 10–50 ms) before calling again to avoid
    /// spinning on an empty queue.
    async fn pop(&self) -> Result<Option<CrawlJob>>;

    /// Current number of pending jobs in the frontier.
    async fn len(&self) -> Result<u64>;

    /// Returns `true` if the frontier is empty.
    async fn is_empty(&self) -> Result<bool> {
        Ok(self.len().await? == 0)
    }
}
