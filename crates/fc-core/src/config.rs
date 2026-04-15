// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Strongly-typed configuration for the entire ferrocrawl system.
//!
//! Configuration is loaded from a TOML file and can be overridden by
//! environment variables prefixed `FC__` (double underscore as separator).
//! All structs implement `Default` so that a minimal config file that only
//! overrides a few keys still produces a fully-populated config.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};

use crate::error::{CrawlError, Result};

// ── Root config ───────────────────────────────────────────────────────────────

/// Root configuration. Deserialised from `crawler.toml` (or the path given
/// by `--config`). Every sub-struct has a `Default` impl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlerConfig {
    #[serde(default)]
    pub crawler: CoreConfig,

    #[serde(default)]
    pub scheduler: SchedulerConfig,

    #[serde(default)]
    pub dedup: DedupConfig,

    #[serde(default)]
    pub rate_limiter: RateLimiterConfig,

    #[serde(default)]
    pub proxy: ProxyConfig,

    #[serde(default)]
    pub export: ExportConfig,

    #[serde(default)]
    pub redis: RedisConfig,

    #[serde(default)]
    pub api: ApiConfig,
}

impl CrawlerConfig {
    /// Loads config from a TOML file, then layers environment variable
    /// overrides on top. The env var separator is `__` (double underscore),
    /// so `FC__CRAWLER__CONCURRENCY=1000` overrides `crawler.concurrency`.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        let builder = config::Config::builder()
            .add_source(config::File::from(path).required(true))
            .add_source(
                config::Environment::with_prefix("FC")
                    .separator("__")
                    .try_parsing(true),
            );

        builder
            .build()
            .and_then(|c| c.try_deserialize::<Self>())
            .map_err(|e| CrawlError::Config(format!("{e}")))
    }

    /// Loads the default config without reading any file. Useful in tests
    /// and single-command invocations that don't need customisation.
    pub fn default_config() -> Self {
        Self::default()
    }
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        Self {
            crawler: CoreConfig::default(),
            scheduler: SchedulerConfig::default(),
            dedup: DedupConfig::default(),
            rate_limiter: RateLimiterConfig::default(),
            proxy: ProxyConfig::default(),
            export: ExportConfig::default(),
            redis: RedisConfig::default(),
            api: ApiConfig::default(),
        }
    }
}

// ── CoreConfig ────────────────────────────────────────────────────────────────

/// Controls the fetch engine: concurrency, timeouts, and politeness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    /// Number of concurrent in-flight HTTP requests across all worker tasks.
    pub concurrency: usize,

    /// Maximum crawl depth from any seed URL. `0` means seeds only.
    pub depth: u32,

    /// HTTP request timeout in seconds.
    pub request_timeout_secs: u64,

    /// `User-Agent` header sent with every request.
    pub user_agent: String,

    /// Whether to follow HTTP 3xx redirects.
    pub follow_redirects: bool,

    /// Maximum number of redirects before giving up.
    pub max_redirects: u32,

    /// Maximum response body size in bytes. Larger bodies are discarded.
    pub max_body_bytes: usize,

    /// Whether to parse and honour `robots.txt` rules.
    pub respect_robots_txt: bool,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            concurrency: 200,
            depth: 3,
            request_timeout_secs: 30,
            user_agent: concat!(
                "ferrocrawl/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/yourusername/ferrocrawl)"
            )
            .to_owned(),
            follow_redirects: true,
            max_redirects: 10,
            max_body_bytes: 10 * 1024 * 1024, // 10 MiB
            respect_robots_txt: true,
        }
    }
}

impl CoreConfig {
    /// Returns `request_timeout_secs` as a [`Duration`].
    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.request_timeout_secs)
    }
}

// ── SchedulerConfig ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// Maximum number of URLs that can be queued at once. When the frontier
    /// is full, new URLs are dropped (not errored) until space is available.
    pub max_queue_size: usize,

    /// How priority is assigned to newly-discovered URLs.
    pub priority_mode: PriorityMode,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_queue_size: 1_000_000,
            priority_mode: PriorityMode::Breadth,
        }
    }
}

/// Controls the ordering of the frontier queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityMode {
    /// BFS — shallower URLs get higher priority. Good for broad coverage.
    Breadth,
    /// DFS — deeper URLs get higher priority. Good for deep-diving a site.
    Depth,
    /// Priority set explicitly on each [`CrawlJob`]. Requires the caller
    /// to set `CrawlJob::priority` before pushing.
    Score,
}

// ── DedupConfig ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupConfig {
    /// Expected number of unique URLs. Sets the Bloom filter's bit array size.
    /// Under-sizing increases the false-positive rate; over-sizing wastes RAM.
    pub bloom_capacity: usize,

    /// Target false-positive rate. `0.001` = 0.1%.
    /// At 100M capacity this requires ~180 MiB of RAM.
    pub bloom_fpr: f64,
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            bloom_capacity: 10_000_000,
            bloom_fpr: 0.001,
        }
    }
}

// ── RateLimiterConfig ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimiterConfig {
    /// Default requests per second per domain.
    pub default_rps: u32,

    /// Burst allowance. The limiter allows up to `burst` requests before
    /// the token bucket must refill. Set to the same value as `default_rps`
    /// for no burst.
    pub burst: u32,

    /// Per-domain RPS overrides. Keys are bare hostnames (e.g. `"example.com"`).
    #[serde(default)]
    pub per_domain: HashMap<String, u32>,
}

impl Default for RateLimiterConfig {
    fn default() -> Self {
        Self {
            default_rps: 10,
            burst: 20,
            per_domain: HashMap::new(),
        }
    }
}

// ── ProxyConfig ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub rotation: ProxyRotation,
    /// Proxy URLs. Supported schemes: `http://`, `https://`, `socks5://`.
    pub proxies: Vec<String>,
    /// Number of consecutive failures before a proxy is temporarily disabled.
    pub failure_threshold: u32,
    /// How long (seconds) a failed proxy is quarantined before retry.
    pub quarantine_secs: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rotation: ProxyRotation::RoundRobin,
            proxies: Vec::new(),
            failure_threshold: 5,
            quarantine_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyRotation {
    /// Cycles through proxies in order.
    RoundRobin,
    /// Picks a proxy at random on each request.
    Random,
    /// Keeps the same proxy for all requests to a given domain.
    StickyDomain,
}

// ── ExportConfig ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfig {
    /// Which sink to write records to.
    pub sink: SinkType,

    /// Output directory for file-based sinks (Parquet, NDJSON).
    pub output_dir: PathBuf,

    /// Number of records to accumulate before writing a Parquet row group.
    /// Larger batches = better compression; smaller = lower latency.
    pub parquet_batch_size: usize,

    #[serde(default)]
    pub clickhouse: ClickHouseConfig,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            sink: SinkType::Ndjson,
            output_dir: PathBuf::from("./output"),
            parquet_batch_size: 50_000,
            clickhouse: ClickHouseConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkType {
    Parquet,
    Ndjson,
    ClickHouse,
    /// Writes JSON lines to stdout. Useful for development and piping.
    Stdout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClickHouseConfig {
    pub url: String,
    pub database: String,
    pub table: String,
    pub username: String,
    pub password: String,
    /// Number of records per HTTP insert batch.
    pub batch_size: usize,
}

impl Default for ClickHouseConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8123".to_owned(),
            database: "crawler".to_owned(),
            table: "records".to_owned(),
            username: "default".to_owned(),
            password: String::new(),
            batch_size: 10_000,
        }
    }
}

// ── RedisConfig ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    pub url: String,
    /// Redis Stream name for the job queue.
    pub stream: String,
    /// Consumer group name. All workers in a deployment share a group.
    pub consumer_group: String,
    /// Seconds before an unacknowledged job is reclaimed and requeued.
    pub visibility_timeout_secs: u64,
    /// Maximum batch size for `XREADGROUP` calls.
    pub read_batch_size: usize,
}

impl Default for RedisConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_owned(),
            stream: "fc:jobs".to_owned(),
            consumer_group: "workers".to_owned(),
            visibility_timeout_secs: 300,
            read_batch_size: 10,
        }
    }
}

// ── ApiConfig ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    pub host: String,
    pub port: u16,
    /// If `Some`, all API requests must include `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
    /// Enable detailed request tracing in the HTTP access log.
    pub trace_requests: bool,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_owned(),
            port: 8080,
            auth_token: None,
            trace_requests: false,
        }
    }
}

impl ApiConfig {
    /// Returns the bind address as a `SocketAddr`-compatible string.
    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
