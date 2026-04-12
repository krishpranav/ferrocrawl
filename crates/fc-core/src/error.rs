// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Unified error taxonomy for the entire ferrocrawl pipeline.
//!
//! All errors are named, structured variants. Callers can branch on the exact
//! failure mode without string matching. The `is_retryable` predicate lets the
//! worker engine decide whether to requeue a job without knowing anything about
//! HTTP semantics itself.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CrawlError {
    // ── Network and HTTP ──────────────────────────────────────────────────
    #[error("HTTP {status} fetching {url}: {message}")]
    Http {
        url: String,
        status: u16,
        message: String,
    },

    #[error("network error fetching `{url}`: {source}")]
    Network {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    #[error("request timeout for `{url}` after {timeout_secs}s")]
    Timeout { url: String, timeout_secs: u64 },

    #[error("redirect loop or limit exceeded for `{url}`")]
    TooManyRedirects { url: String },

    #[error("robots.txt disallows crawling `{url}`")]
    RobotsDisallowed { url: String },

    // ── Parsing and extraction ────────────────────────────────────────────
    #[error("parse error for `{url}`: {message}")]
    Parse { url: String, message: String },

    #[error("extraction error: {0}")]
    Extraction(String),

    // ── Scheduler and dedup ───────────────────────────────────────────────
    #[error("scheduler error: {0}")]
    Scheduler(String),

    #[error("bloom filter capacity exhausted — increase `dedup.bloom_capacity`")]
    BloomCapacityExceeded,

    // ── Export and I/O ────────────────────────────────────────────────────
    #[error("export error: {0}")]
    Export(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    // ── Infrastructure ────────────────────────────────────────────────────
    #[error("redis error: {0}")]
    Redis(String),

    #[error("worker error: {0}")]
    Worker(String),

    // ── Configuration ─────────────────────────────────────────────────────
    #[error("configuration error: {0}")]
    Config(String),

    // ── Type conversions ──────────────────────────────────────────────────
    #[error("invalid URL `{0}`: {1}")]
    UrlParse(String, #[source] url::ParseError),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    // ── Control flow ──────────────────────────────────────────────────────
    #[error("graceful shutdown requested")]
    Shutdown,
}

/// Alias so every `fc-*` crate can write `Result<T>` without qualification.
pub type Result<T, E = CrawlError> = std::result::Result<T, E>;

impl CrawlError {
    /// Returns `true` if retrying this request has a reasonable chance of
    /// succeeding. The worker engine uses this to decide whether to requeue
    /// or to discard a failed job.
    ///
    /// Transient conditions: network errors, timeouts, HTTP 429 (rate-limited),
    /// and any HTTP 5xx (server-side fault).
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { .. } | Self::Timeout { .. } => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }

    /// Returns the URL associated with this error, if any.
    ///
    /// Useful for structured logging without pattern-matching on every variant.
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Http { url, .. }
            | Self::Network { url, .. }
            | Self::Timeout { url, .. }
            | Self::TooManyRedirects { url }
            | Self::RobotsDisallowed { url }
            | Self::Parse { url, .. } => Some(url.as_str()),
            Self::UrlParse(url, _) => Some(url.as_str()),
            _ => None,
        }
    }

    /// Wraps an arbitrary network-layer error with its originating URL.
    pub fn network(url: impl Into<String>, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Network {
            url: url.into(),
            source: Box::new(source),
        }
    }
}