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

pub struct ParsedPage {
    pub document: Html,
    pub links: Vec<Url>,
    pub base_url: Url,
    pub title: Option<String>,
    pub status: u16,
}

pub struct HtmlParser {
    sel_a: Selector,
    sel_base: Selector,
    sel_title: Selector,
}

impl HtmlParser {
    pub fn new() -> Self {
        Self {
            sel_a: Selector::parse("a[href]").expect("selector 'a[href]' is valid"),
            sel_base: Selector::parse("base[href]").expect("selector 'base[href]' is valid"),
            sel_title: Selector::parse("title").expect("selector 'title' is valid"),
        }
    }
}