// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Core domain types for the ferrocrawl pipeline.
//!
//! The data model follows a straightforward pipeline shape:
//!
//! ```text
//! CrawlJob  →  FetchResponse  →  ExtractedRecord  →  (sink)
//!                                      ↓
//!                              discovered_links  →  CrawlJob (new)
//! ```
//!
//! All types implement `Send + Sync` and the serialisable ones implement
//! `serde::{Serialize, Deserialize}` for wire transport and Parquet export.

use std::collections::HashMap;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

// ── Priority ──────────────────────────────────────────────────────────────────

/// Scheduling priority for a [`CrawlJob`].
///
/// Represented as a `u8` so it fits in a single byte on the wire and can be
/// used as a key in the Redis stream. Higher values = higher priority.
/// The [`BinaryHeap`][std::collections::BinaryHeap] in the frontier pops
/// maximum values first, so `Priority::CRITICAL` jobs are always fetched next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Priority(pub u8);

impl Priority {
    pub const LOW: Self = Self(32);
    pub const NORMAL: Self = Self(128);
    pub const HIGH: Self = Self(192);
    pub const CRITICAL: Self = Self(255);
}

impl Default for Priority {
    fn default() -> Self {
        Self::NORMAL
    }
}

// ── CrawlJob ──────────────────────────────────────────────────────────────────

/// A unit of work dispatched to a worker.
///
/// `CrawlJob` is the currency of the entire system — it flows through the
/// frontier queue, gets serialised into Redis, and arrives at workers. Every
/// field is cheap to clone because the struct is sent across thread and process
/// boundaries frequently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlJob {
    /// Unique identifier. Used for deduplication at the job level and for
    /// correlating log lines across workers.
    pub id: Uuid,

    /// The URL to fetch. Normalised by the scheduler on insert.
    pub url: Url,

    /// Crawl depth from the original seed URL. The engine stops enqueuing
    /// new links once this reaches `CrawlerConfig::depth`.
    pub depth: u32,

    /// Scheduling priority in the frontier. Computed by the scheduler from
    /// the configured [`PriorityMode`][crate::config::PriorityMode].
    pub priority: Priority,

    /// The extractor schema to apply. `None` means run all matching schemas.
    pub schema_name: Option<String>,

    /// Number of times this job has been attempted. Incremented by the worker
    /// engine on each failure before requeueing.
    pub attempt: u32,

    /// Maximum retries before the job is discarded. Set by the engine.
    pub max_attempts: u32,

    /// When this job was created, for latency tracking.
    pub created_at: DateTime<Utc>,

    /// The URL that contained a link to this one. `None` for seed URLs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referrer: Option<Url>,

    /// Arbitrary key-value metadata for domain-specific context
    /// (e.g. Solana program address, product category).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

impl CrawlJob {
    /// Constructs a new job with sane defaults.
    pub fn new(url: Url, depth: u32) -> Self {
        Self {
            id: Uuid::new_v4(),
            url,
            depth,
            priority: Priority::default(),
            schema_name: None,
            attempt: 0,
            max_attempts: 3,
            created_at: Utc::now(),
            referrer: None,
            metadata: HashMap::new(),
        }
    }

    // ── Builder-style setters ─────────────────────────────────────────────

    #[must_use]
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    #[must_use]
    pub fn with_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema_name = Some(schema.into());
        self
    }

    #[must_use]
    pub fn with_referrer(mut self, referrer: Url) -> Self {
        self.referrer = Some(referrer);
        self
    }

    #[must_use]
    pub fn with_max_attempts(mut self, max: u32) -> Self {
        self.max_attempts = max;
        self
    }

    #[must_use]
    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    // ── Derived properties ────────────────────────────────────────────────

    /// The effective domain, used as the rate-limiter key.
    pub fn domain(&self) -> Option<&str> {
        self.url.host_str()
    }

    /// Returns `true` if this job has exceeded its retry budget.
    pub fn is_exhausted(&self) -> bool {
        self.attempt >= self.max_attempts
    }

    /// Returns a clone with `attempt` incremented, ready to requeue.
    pub fn next_attempt(&self) -> Self {
        let mut next = self.clone();
        next.attempt += 1;
        next
    }
}

// ── FetchResponse ─────────────────────────────────────────────────────────────

/// The raw result of a successful HTTP fetch.
///
/// Not serialised — it lives only within a single worker's memory, long enough
/// to be parsed and extracted from.
#[derive(Debug)]
pub struct FetchResponse {
    /// The URL that was originally requested.
    pub url: Url,
    /// The final URL after following any redirects.
    pub final_url: Url,
    /// HTTP status code.
    pub status: u16,
    /// Normalised response headers (lowercase names).
    pub headers: HashMap<String, String>,
    /// Raw response body.
    pub body: Bytes,
    /// Wall-clock time for the network round-trip.
    pub fetch_duration: Duration,
}

impl FetchResponse {
    /// Returns the `Content-Type` header value, if present.
    pub fn content_type(&self) -> Option<&str> {
        self.headers.get("content-type").map(String::as_str)
    }

    /// Returns `true` if the response body is HTML.
    pub fn is_html(&self) -> bool {
        self.content_type()
            .map(|ct| ct.contains("text/html"))
            .unwrap_or(false)
    }

    /// Returns `true` if the response body is JSON.
    pub fn is_json(&self) -> bool {
        self.content_type()
            .map(|ct| ct.contains("application/json"))
            .unwrap_or(false)
    }

    /// Returns `true` if the status code indicates success (2xx).
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Attempt to decode the body as UTF-8 text.
    pub fn text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.body)
    }
}

// ── FieldValue ────────────────────────────────────────────────────────────────

/// A dynamically-typed field value produced by the extractor.
///
/// The `#[serde(untagged)]` attribute means values serialise cleanly as native
/// JSON types, so `FieldValue::Integer(42)` → `42` in the output, not
/// `{"Integer": 42}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    Text(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    List(Vec<FieldValue>),
    Null,
}

impl FieldValue {
    /// Returns the contained string, or `None` if this is not a `Text` variant.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// Returns `true` if this is the `Null` variant.
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

impl From<String> for FieldValue {
    fn from(s: String) -> Self { Self::Text(s) }
}
impl From<&str> for FieldValue {
    fn from(s: &str) -> Self { Self::Text(s.to_owned()) }
}
impl From<i64> for FieldValue {
    fn from(n: i64) -> Self { Self::Integer(n) }
}
impl From<i32> for FieldValue {
    fn from(n: i32) -> Self { Self::Integer(n as i64) }
}
impl From<f64> for FieldValue {
    fn from(f: f64) -> Self { Self::Float(f) }
}
impl From<bool> for FieldValue {
    fn from(b: bool) -> Self { Self::Bool(b) }
}
impl<T: Into<FieldValue>> From<Option<T>> for FieldValue {
    fn from(opt: Option<T>) -> Self {
        opt.map(Into::into).unwrap_or(Self::Null)
    }
}
impl<T: Into<FieldValue>> From<Vec<T>> for FieldValue {
    fn from(vec: Vec<T>) -> Self {
        Self::List(vec.into_iter().map(Into::into).collect())
    }
}

// ── ExtractedRecord ───────────────────────────────────────────────────────────

/// A structured record produced by the extractor, ready to be written to a sink.
///
/// One record corresponds to one "thing" extracted from one page — a product,
/// an article, a token, a DEX pool. The `schema` field identifies which
/// extraction rules produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRecord {
    /// The name of the schema that produced this record.
    pub schema: String,

    /// Canonical URL of the page this was extracted from.
    pub source_url: String,

    /// UTC timestamp of when extraction completed.
    pub crawled_at: DateTime<Utc>,

    /// The extracted field values, keyed by the field names defined in the schema.
    pub fields: HashMap<String, FieldValue>,

    /// Links discovered on the page. The scheduler uses these to expand the frontier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovered_links: Vec<String>,
}

impl ExtractedRecord {
    /// Constructs an empty record for the given schema and source URL.
    pub fn new(schema: impl Into<String>, source_url: impl Into<String>) -> Self {
        Self {
            schema: schema.into(),
            source_url: source_url.into(),
            crawled_at: Utc::now(),
            fields: HashMap::new(),
            discovered_links: Vec::new(),
        }
    }

    /// Inserts a field value. Accepts anything that converts to [`FieldValue`].
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<FieldValue>) {
        self.fields.insert(key.into(), value.into());
    }

    /// Returns the value for a given field name.
    pub fn get(&self, key: &str) -> Option<&FieldValue> {
        self.fields.get(key)
    }

    /// Appends a discovered link, normalised to a string.
    pub fn add_link(&mut self, url: impl Into<String>) {
        self.discovered_links.push(url.into());
    }
}

// ── CrawlResult ───────────────────────────────────────────────────────────────

/// The complete outcome of processing a single [`CrawlJob`].
///
/// Produced by the worker engine and consumed by the coordinator to update
/// stats, feed the frontier with new URLs, and route records to the sink.
#[derive(Debug)]
pub struct CrawlResult {
    /// The job that produced this result.
    pub job: CrawlJob,

    /// The extracted record, if extraction succeeded.
    pub record: Option<ExtractedRecord>,

    /// New URLs to enqueue. Filtered by depth and dedup before insertion.
    pub discovered_urls: Vec<Url>,

    /// High-level outcome classification.
    pub status: CrawlStatus,

    /// Time spent on the network round-trip.
    pub fetch_duration: Duration,

    /// Total wall-clock time from job start to result.
    pub total_duration: Duration,
}

/// High-level classification of a crawl outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CrawlStatus {
    Success,
    HttpError(u16),
    ParseError(String),
    ExtractionError(String),
    Skipped(SkipReason),
    Failed { message: String, retryable: bool },
}

impl CrawlStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }
}

/// Reason a job was skipped without fetching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkipReason {
    Duplicate,
    RobotsDisallowed,
    DepthExceeded,
    DomainBlocked,
    ContentTypeMismatch,
    AttemptLimitExceeded,
}

// ── CrawlStats ────────────────────────────────────────────────────────────────

/// Aggregate statistics for a running or completed crawl job.
///
/// Exposed via the Prometheus exporter and the `/jobs/:id` REST endpoint.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CrawlStats {
    pub pages_fetched: u64,
    pub pages_failed: u64,
    pub pages_skipped: u64,
    pub records_extracted: u64,
    pub urls_discovered: u64,
    pub bytes_downloaded: u64,
    pub queue_depth: u64,
    pub elapsed_secs: f64,
}

impl CrawlStats {
    /// Current throughput in pages per second.
    pub fn pages_per_second(&self) -> f64 {
        if self.elapsed_secs > 0.0 {
            self.pages_fetched as f64 / self.elapsed_secs
        } else {
            0.0
        }
    }

    /// Fraction of fetch attempts that succeeded (0.0 – 1.0).
    pub fn success_rate(&self) -> f64 {
        let total = self.pages_fetched + self.pages_failed;
        if total > 0 { self.pages_fetched as f64 / total as f64 } else { 1.0 }
    }

    /// Total pages processed (fetched + failed + skipped).
    pub fn total_processed(&self) -> u64 {
        self.pages_fetched + self.pages_failed + self.pages_skipped
    }
}