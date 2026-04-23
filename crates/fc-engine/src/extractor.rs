// SPDX-License-Identifier: MIT
// Copyright (c) 2025 ferrocrawl contributors
//
// See LICENSE in the project root for full license terms.

//! Schema-driven field extraction from parsed HTML documents.
//!
//! ## Schema definition
//!
//! Schemas are defined in TOML files. Each schema names a set of CSS-selector
//! rules that map elements to typed fields:
//!
//! ```toml
//! [[extractor]]
//! name        = "product"
//! url_pattern = "shop\\.example\\.com/products/"
//!
//! [[extractor.fields]]
//! name      = "title"
//! selector  = "h1.product-name"
//! attr      = "text"         # "text" = inner text, any other value = HTML attr
//!
//! [[extractor.fields]]
//! name      = "price"
//! selector  = "span[data-price]"
//! attr      = "data-price"
//! transform = "parse_float"
//!
//! [[extractor.fields]]
//! name      = "images"
//! selector  = "img.product-image"
//! attr      = "src"
//! multiple  = true           # collect all matches as a list
//! ```
//!
//! ## Registry
//!
//! [`SchemaRegistry`] loads all `*.toml` schema files from a directory and
//! dispatches incoming [`FetchResponse`]s to every schema whose `url_pattern`
//! matches the response URL. Multiple schemas can match a single response —
//! each produces an independent [`ExtractedRecord`].
use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use fc_core::{
    error::{CrawlError, Result},
    traits::Extractor,
    types::{ExtractedRecord, FieldValue, FetchResponse},
};
use regex::Regex;
use scraper::{Html, Selector};
use serde::Deserialize;
use tracing::{debug, instrument, warn};

#[derive(Debug, Deserialize)]
struct SchemaFile {
    extractor: Vec<ExtractorSchema>,
}

#[derive(Debug, Deserialize)]
struct ExtractorSchema {
    name: String,
    url_pattern: Option<String>,

    #[serde(default)]
    fields: Vec<FieldRule>,
}

#[derive(Debug, Deserialize)]
struct FieldRule {
    name: String,
    selector: String,

    #[serde(default = "FieldRule::default_attr")]
    attr: String,

    transform: Option<String>,

    #[serde(default)]
    multiple: bool,

    #[serde(default)]
    optional: bool,
}

impl FieldRule {
    fn default_attr() -> String {
        "text".to_owned()
    }
}