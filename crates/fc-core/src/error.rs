// SPDX-License-Identifier: MIT
// Copyright (c) 2025 Krisna Pranav, ferrocrawl contributors
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
    #[error("HTP {status} fetching {url}: {message}")]
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
}

pub type Result<T, E = CrawlError> = std::result::Result<T, E>;

impl CrawlError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { .. } | Self::Timeout { .. } => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false
        }
    }

    pub fn network(url: impl Into<String>, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Network {
            url: url.into(),
            source: Box::new(source),
        }
    }
}