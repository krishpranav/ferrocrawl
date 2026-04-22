// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! HTML parser — turns a [`FetchResponse`] body into a queryable DOM and a
//! deduplicated list of absolute outbound links.
//!
//! ## Design
//!
//! `HtmlParser` is a zero-state struct. Every call to `parse` creates a fresh
//! [`scraper::Html`] document from the response bytes. There is no caching —
//! the parsed DOM lives only long enough for the extractor to walk it, then
//! it is dropped.
//!
//! Link discovery normalises URLs before returning:
//! - Relative URLs are resolved against the document's final URL.
//! - Fragment-only URLs (e.g. `#section`) are discarded.
//! - `javascript:` and `mailto:` schemes are discarded.
//! - Duplicate URLs (post-normalisation) are deduplicated.
//! - `<base href>` is respected when resolving relative URLs.

use std::collections::HashSet;

use fc_core::{
    error::{CrawlError, Result},
    types::FetchResponse,
};
use scraper::{Html, Selector};
use tracing::{debug, trace};
use url::Url;

// ── ParsedPage ────────────────────────────────────────────────────────────────

/// The output of the HTML parser: a parsed document plus discovered links.
///
/// `ParsedPage` does not hold the `Html` document itself — `scraper::Html` is
/// not `Send`, so it cannot cross an `await` point. Callers extract data from
/// the `Html` synchronously before any async work.
pub struct ParsedPage {
    /// The parsed HTML document, ready for selector queries.
    pub document: Html,

    /// Absolute, normalised outbound URLs discovered in `<a href>` attributes.
    /// Filtered, deduplicated, and resolved against `base_url`.
    pub links: Vec<Url>,

    /// The effective base URL for the document. This is either the `<base href>`
    /// value (if present and valid) or `response.final_url`.
    pub base_url: Url,

    /// The document title (`<title>` text content), if present.
    pub title: Option<String>,

    /// HTTP status code of the originating response.
    pub status: u16,
}

// ── HtmlParser ────────────────────────────────────────────────────────────────

/// Stateless HTML parser. Construct once and reuse — it holds no mutable state.
pub struct HtmlParser {
    /// Pre-compiled selectors, built once at construction time so they are not
    /// re-compiled on every document parse.
    sel_a: Selector,
    sel_base: Selector,
    sel_title: Selector,
}

impl HtmlParser {
    /// Creates a new parser with pre-compiled selectors.
    ///
    /// # Panics
    ///
    /// Panics if the hard-coded selector strings are invalid CSS. They are
    /// compile-time constants, so this would be a programming error.
    pub fn new() -> Self {
        Self {
            sel_a: Selector::parse("a[href]").expect("selector 'a[href]' is valid"),
            sel_base: Selector::parse("base[href]").expect("selector 'base[href]' is valid"),
            sel_title: Selector::parse("title").expect("selector 'title' is valid"),
        }
    }

    /// Parses an HTML response into a [`ParsedPage`].
    ///
    /// Returns `Err` only if the response is not HTML. The HTML parser itself
    /// is infallible — `scraper` accepts malformed markup gracefully, just as
    /// browsers do.
    pub fn parse(&self, response: &FetchResponse) -> Result<ParsedPage> {
        if !response.is_html() {
            return Err(CrawlError::Parse {
                url: response.url.to_string(),
                message: format!(
                    "expected text/html, got {:?}",
                    response.content_type().unwrap_or("unknown")
                ),
            });
        }

        let html_str = response
            .text()
            .map_err(|_| CrawlError::Parse {
                url: response.url.to_string(),
                message: "response body is not valid UTF-8".to_owned(),
            })?;

        let document = Html::parse_document(html_str);

        // Resolve `<base href>` if present and valid.
        let base_url = self.resolve_base_url(&document, &response.final_url);

        let title = self.extract_title(&document);
        let links = self.extract_links(&document, &base_url);

        debug!(
            url = %response.final_url,
            links = links.len(),
            title = title.as_deref().unwrap_or("<none>"),
            "parsed HTML document"
        );

        Ok(ParsedPage {
            document,
            links,
            base_url,
            title,
            status: response.status,
        })
    }

    // ── Private helpers ───────────────────────────────────────────────────

    /// Resolves the effective base URL for the document.
    fn resolve_base_url(&self, document: &Html, final_url: &Url) -> Url {
        document
            .select(&self.sel_base)
            .next()
            .and_then(|el| el.value().attr("href"))
            .and_then(|href| final_url.join(href).ok())
            .unwrap_or_else(|| final_url.clone())
    }

    /// Extracts the document title.
    fn extract_title(&self, document: &Html) -> Option<String> {
        document
            .select(&self.sel_title)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_owned())
            .filter(|t| !t.is_empty())
    }

    /// Extracts, resolves, and deduplicates all outbound links.
    fn extract_links(&self, document: &Html, base_url: &Url) -> Vec<Url> {
        let mut seen = HashSet::new();
        let mut links = Vec::new();

        for element in document.select(&self.sel_a) {
            let href = match element.value().attr("href") {
                Some(h) => h.trim(),
                None => continue,
            };

            // Discard fragment-only, javascript:, and mailto: URLs.
            if href.is_empty() || href.starts_with('#') {
                continue;
            }

            if href.starts_with("javascript:") || href.starts_with("mailto:") {
                trace!(href, "discarding non-http link");
                continue;
            }

            // Resolve relative URLs.
            let url = match base_url.join(href) {
                Ok(u) => u,
                Err(e) => {
                    trace!(href, error = %e, "failed to resolve URL; skipping");
                    continue;
                }
            };

            // Only follow http(s) links.
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }

            // Normalise: strip fragment.
            let mut normalised = url;
            normalised.set_fragment(None);

            // Deduplicate.
            if seen.insert(normalised.to_string()) {
                links.push(normalised);
            }
        }

        links
    }
}

impl Default for HtmlParser {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::time::Duration;

    fn make_response(url: &str, body: &str) -> FetchResponse {
        FetchResponse {
            url: url.parse().unwrap(),
            final_url: url.parse().unwrap(),
            status: 200,
            headers: {
                let mut h = HashMap::new();
                h.insert("content-type".to_owned(), "text/html; charset=utf-8".to_owned());
                h
            },
            body: Bytes::from(body.to_owned()),
            fetch_duration: Duration::ZERO,
        }
    }

    #[test]
    fn extracts_absolute_links() {
        let parser = HtmlParser::new();
        let html = "<html><body> </body><html>";

        let response = make_response("https://example.com/", html);
        let page = parser.parse(&response).unwrap();

        assert_eq!(page.links.len(), 2);
        assert!(page.links.iter().any(|u| u.as_str() == "https://other.com/page"));
        assert!(page.links.iter().any(|u| u.as_str() == "https://example.com/relative"));
    }

    #[test]
    fn deduplicates_links() {
        let parser = HtmlParser::new();
        let html = r#"
            <html><body>
                <a href="/page">1</a>
                <a href="/page">2</a>
                <a href="/page#section">3</a>
            </body></html>
        "#;

        let response = make_response("https://example.com/", html);
        let page = parser.parse(&response).unwrap();

        // All three resolve to the same URL after fragment stripping.
        assert_eq!(page.links.len(), 1);
    }

    #[test]
    fn extracts_title() {
        let parser = HtmlParser::new();
        let html = "<html><head><title>  My Page  </title></head><body></body></html>";

        let response = make_response("https://example.com/", html);
        let page = parser.parse(&response).unwrap();

        assert_eq!(page.title.as_deref(), Some("My Page"));
    }

    #[test]
    fn respects_base_href() {
        let parser = HtmlParser::new();
        let html = r#"
            <html><head><base href="https://cdn.example.com/assets/"/></head>
            <body><a href="style.css">link</a></body></html>
        "#;

        let response = make_response("https://example.com/", html);
        let page = parser.parse(&response).unwrap();

        // "style.css" resolved against the <base> URL.
        assert!(page.links.iter().any(|u| {
            u.as_str() == "https://cdn.example.com/assets/style.css"
        }));
    }

    #[test]
    fn rejects_non_html_response() {
        let parser = HtmlParser::new();
        let mut response = make_response("https://example.com/api", r#"{"key":"value"}"#);
        response.headers.insert(
            "content-type".to_owned(),
            "application/json".to_owned(),
        );

        let result = parser.parse(&response);
        assert!(result.is_err());
    }
}