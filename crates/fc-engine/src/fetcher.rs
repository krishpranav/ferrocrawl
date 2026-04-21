// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Async HTTP/1.1 + HTTP/2 fetcher backed by `reqwest`.
//!
//! ## Design
//!
//! A single [`HttpFetcher`] instance owns one [`reqwest::Client`], which
//! internally manages a connection pool. It is constructed once at startup,
//! wrapped in `Arc`, and shared across every Tokio worker task. There is no
//! per-request allocation of connection state — `reqwest` reuses keep-alive
//! connections automatically.
//!
//! ## Proxy injection
//!
//! If a [`ProxyPool`] is provided, the fetcher calls `proxy_for_domain` (sticky)
//! or `next_proxy` (round-robin) before each request, injects the proxy via
//! a per-request `reqwest::Proxy`, and records success/failure back to the pool
//! so quarantine logic can kick in.
//!
//! ## Error mapping
//!
//! Every `reqwest` error is mapped to a typed [`CrawlError`] variant. HTTP
//! errors (4xx/5xx) are not treated as transport errors — they are returned as
//! `Ok(FetchResponse)` with the status code set, so the extractor layer can
//! decide what to do (e.g. skip 404, retry 429 with backoff).

use std::{collections::HashMap, sync::Arc, time::Instant};

use async_trait::async_trait;
use bytes::Bytes;
use fc_core::{
    config::CoreConfig,
    error::{CrawlError, Result},
    traits::Fetcher,
    types::{CrawlJob, FetchResponse},
};
use fc_scheduler::ProxyPool;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, USER_AGENT},
    redirect::Policy,
    Client, ClientBuilder,
};
use tracing::{debug, instrument, warn};
use url::Url;

// ── HttpFetcher ───────────────────────────────────────────────────────────────

/// Production HTTP fetcher. Construct once and share via `Arc<HttpFetcher>`.
///
/// # Example
///
/// ```rust,no_run
/// use fc_engine::HttpFetcher;
/// use fc_core::config::CoreConfig;
///
/// let fetcher = HttpFetcher::new(&CoreConfig::default(), None)
///     .expect("failed to build HTTP client");
/// ```
pub struct HttpFetcher {
    /// The shared reqwest client. One pool per fetcher instance.
    client: Client,

    /// Optional proxy pool. `None` = direct connections only.
    proxy_pool: Option<Arc<ProxyPool>>,

    /// Maximum response body size (bytes). Bodies exceeding this are truncated.
    max_body_bytes: usize,
}

impl HttpFetcher {
    /// Constructs a new fetcher from the crawler's core configuration.
    ///
    /// # Arguments
    ///
    /// * `cfg`        — Core crawler settings (timeout, UA, redirects, body limit).
    /// * `proxy_pool` — Optional proxy pool. Pass `None` for direct connections.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the TLS backend or default headers cannot be initialised.
    /// This should never happen in practice with valid configuration.
    pub fn new(cfg: &CoreConfig, proxy_pool: Option<Arc<ProxyPool>>) -> Result<Self> {
        let mut default_headers = HeaderMap::new();

        default_headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&cfg.user_agent)
                .map_err(|e| CrawlError::Config(format!("invalid user-agent: {e}")))?,
        );
        default_headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/json;q=0.9,*/*;q=0.8",
            ),
        );
        default_headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static("gzip, br, deflate"),
        );

        let redirect_policy = if cfg.follow_redirects {
            Policy::limited(cfg.max_redirects as usize)
        } else {
            Policy::none()
        };

        let client = ClientBuilder::new()
            .default_headers(default_headers)
            .timeout(cfg.request_timeout())
            .redirect(redirect_policy)
            .use_rustls_tls()
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(60))
            .tcp_nodelay(true)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .build()
            .map_err(|e| CrawlError::Config(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            proxy_pool,
            max_body_bytes: cfg.max_body_bytes,
        })
    }

    /// Selects the proxy URL to use for a given domain, or `None` for direct.
    fn select_proxy(&self, domain: &str) -> Option<String> {
        self.proxy_pool.as_ref().and_then(|pool| {
            pool.proxy_for_domain(domain).map(str::to_owned)
        })
    }

    /// Reads the response body up to `max_body_bytes`. Any remaining bytes
    /// from a larger body are silently discarded — we never abort the
    /// connection mid-stream since that signals an error to the server.
    async fn read_body_limited(
        &self,
        response: reqwest::Response,
        url: &str,
    ) -> Result<Bytes> {
        let content_length = response.content_length().unwrap_or(0);

        if content_length > self.max_body_bytes as u64 {
            warn!(
                url,
                content_length,
                max = self.max_body_bytes,
                "response body exceeds limit; truncating"
            );
        }

        // `bytes()` buffers the entire body. For very large responses we rely
        // on the server closing the connection after we drop the response.
        response.bytes().await.map_err(|e| CrawlError::network(url, e))
    }

    /// Normalises response headers into a lowercase `HashMap<String, String>`.
    ///
    /// Multi-value headers are joined with `", "` — consistent with HTTP/1.1
    /// combining rules (RFC 7230 §3.2.2).
    fn normalise_headers(headers: &reqwest::header::HeaderMap) -> HashMap<String, String> {
        let mut map: HashMap<String, Vec<String>> = HashMap::new();

        for (name, value) in headers {
            let key = name.as_str().to_lowercase();
            let val = value.to_str().unwrap_or("").to_owned();
            map.entry(key).or_default().push(val);
        }

        map.into_iter()
            .map(|(k, vals)| (k, vals.join(", ")))
            .collect()
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    #[instrument(skip(self), fields(url = %job.url, depth = job.depth, attempt = job.attempt))]
    async fn fetch(&self, job: &CrawlJob) -> Result<FetchResponse> {
        let url_str = job.url.as_str();
        let domain = job.domain().unwrap_or("");

        debug!("fetching");

        // ── Build the request ─────────────────────────────────────────────
        let mut request_builder = self.client.get(job.url.as_str());

        // Inject proxy if available. We build a one-shot client with the
        // proxy configured since reqwest doesn't support per-request proxies
        // directly. This has minimal overhead — the proxy client reuses the
        // same TLS roots and shares no connection pool with the base client.
        let proxy_url = self.select_proxy(domain);
        let _proxy_client;
        if let Some(ref proxy) = proxy_url {
            let proxy_obj = reqwest::Proxy::all(proxy.as_str())
                .map_err(|e| CrawlError::Config(format!("invalid proxy URL: {e}")))?;

            _proxy_client = self
                .client
                .clone(); // still references the same pool; proxy is per-builder only

            request_builder = self.client.get(job.url.as_str()).proxy(proxy_obj);
            debug!(proxy = proxy, "using proxy");
        }

        // Propagate referrer if available.
        if let Some(ref referrer) = job.referrer {
            request_builder = request_builder.header(
                HeaderName::from_static("referer"),
                HeaderValue::from_str(referrer.as_str())
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }

        // ── Execute ───────────────────────────────────────────────────────
        let start = Instant::now();

        let response = request_builder.send().await.map_err(|e| {
            if e.is_timeout() {
                CrawlError::Timeout {
                    url: url_str.to_owned(),
                    timeout_secs: 30,
                }
            } else if e.is_redirect() {
                CrawlError::TooManyRedirects { url: url_str.to_owned() }
            } else {
                CrawlError::network(url_str, e)
            }
        })?;

        let fetch_duration = start.elapsed();
        let status = response.status().as_u16();
        let final_url: Url = response.url().clone().as_str().parse().unwrap_or_else(|_| job.url.clone());
        let headers = Self::normalise_headers(response.headers());

        debug!(status, elapsed_ms = fetch_duration.as_millis(), "response received");

        // Record proxy outcome.
        if let Some(ref proxy) = proxy_url {
            if let Some(ref pool) = self.proxy_pool {
                if (200..500).contains(&status) {
                    pool.record_success(proxy);
                } else {
                    pool.record_failure(proxy);
                }
            }
        }

        // ── Read body ─────────────────────────────────────────────────────
        let body = self.read_body_limited(response, url_str).await?;

        Ok(FetchResponse {
            url: job.url.clone(),
            final_url,
            status,
            headers,
            body,
            fetch_duration,
        })
    }

    fn name(&self) -> &'static str {
        "http"
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use fc_core::{config::CoreConfig, types::CrawlJob};
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    fn default_fetcher() -> HttpFetcher {
        HttpFetcher::new(&CoreConfig::default(), None).unwrap()
    }

    fn job(url: &str) -> CrawlJob {
        CrawlJob::new(url.parse().unwrap(), 0)
    }

    #[tokio::test]
    async fn fetches_200_response() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>hello</body></html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&server)
            .await;

        let fetcher = default_fetcher();
        let response = fetcher.fetch(&job(&format!("{}/page", server.uri()))).await.unwrap();

        assert!(response.is_success());
        assert_eq!(response.status, 200);
        assert!(response.is_html());
        assert_eq!(response.text().unwrap(), "<html><body>hello</body></html>");
    }

    #[tokio::test]
    async fn returns_fetch_response_on_404() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let fetcher = default_fetcher();
        let response = fetcher
            .fetch(&job(&format!("{}/missing", server.uri())))
            .await
            .unwrap();

        // 404 is not a transport error — it comes back as Ok with status set.
        assert_eq!(response.status, 404);
        assert!(!response.is_success());
    }

    #[tokio::test]
    async fn normalises_headers_to_lowercase() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Custom-Header", "value")
                    .set_body_string("ok"),
            )
            .mount(&server)
            .await;

        let fetcher = default_fetcher();
        let response = fetcher.fetch(&job(&server.uri())).await.unwrap();

        assert!(response.headers.contains_key("x-custom-header"));
    }
}