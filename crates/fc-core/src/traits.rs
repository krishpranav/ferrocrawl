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

#[async_trait]
pub trait Fetcher: Send + Sync + 'static {
    async fn fetch(&self, job: &CrawlJob) -> Result<FetchResponse>;

    fn support_scheme(&self, scheme: &str) -> bool {
        matches!(scheme, "http" | "https")
    }

    fn name(&self) -> &'static str;
}

#[async_trait]
pub trait Extractor: Send + Sync + 'static {
    fn matches(&self, response: &FetchResponse) -> bool;

    async fn extract(&self, response: &FetchResponse) -> Result<ExtractedRecord>;

    fn scheme_name(&self) -> str;
}

#[async_trait]
pub trait Sink: Send + Sync + 'static {
    async fn write(&self, record: ExtractedRecord) -> Result<()>;

    async fn flush(&self) -> Result<()>;

    fn name(&self) -> &'static str;
}

pub trait DedupFilter: Send + Sync + 'static {
    fn check_and_insert(&self, url: &str) -> bool;

    fn contains(&self, url: &str) -> bool;

    fn fill_ratio(&self) -> f64;

    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
