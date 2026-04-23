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

// ── Schema TOML types ─────────────────────────────────────────────────────────

/// Root of a schema TOML file. A single file may define multiple extractors.
#[derive(Debug, Deserialize)]
struct SchemaFile {
    extractor: Vec<ExtractorSchema>,
}

/// One named extractor with a URL pattern and a set of field rules.
#[derive(Debug, Deserialize)]
struct ExtractorSchema {
    /// Human-readable name, used as the `schema` field in output records.
    name: String,

    /// A Rust regex matched against the full request URL.
    /// If absent, the extractor matches every URL.
    url_pattern: Option<String>,

    /// Field extraction rules, applied in order.
    #[serde(default)]
    fields: Vec<FieldRule>,
}

/// One field rule: selector + attribute + optional transform.
#[derive(Debug, Deserialize)]
struct FieldRule {
    /// Output field name in the [`ExtractedRecord`].
    name: String,

    /// CSS selector to locate the element(s) in the DOM.
    selector: String,

    /// Which part of the element to extract.
    /// `"text"` → inner text content.
    /// Any other value → the named HTML attribute (e.g. `"href"`, `"src"`).
    #[serde(default = "FieldRule::default_attr")]
    attr: String,

    /// Optional post-extraction transform.
    transform: Option<String>,

    /// If `true`, collect all matching elements into a `FieldValue::List`.
    /// If `false` (default), only the first match is used.
    #[serde(default)]
    multiple: bool,

    /// If `true`, the field is omitted from the record when no element matches.
    /// If `false` (default), a `FieldValue::Null` is inserted.
    #[serde(default)]
    optional: bool,
}

impl FieldRule {
    fn default_attr() -> String {
        "text".to_owned()
    }
}

// ── FieldTransform ────────────────────────────────────────────────────────────

/// Post-extraction value transforms applied to raw string values.
///
/// Transforms are named in the schema TOML and resolved at extraction time.
/// Adding a new transform requires only adding a variant here and a match arm
/// in `apply`.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldTransform {
    /// Parse the string as a 64-bit integer.
    ParseInt,
    /// Parse the string as a 64-bit float.
    ParseFloat,
    /// Parse the string as a boolean (`"true"` / `"false"` / `"1"` / `"0"`).
    ParseBool,
    /// Strip leading and trailing whitespace (applied automatically; explicit
    /// use normalises inner whitespace too via `split_whitespace`).
    Trim,
    /// Convert to lowercase.
    Lowercase,
    /// Convert to uppercase.
    Uppercase,
    /// Remove all non-numeric characters, then parse as integer.
    /// Useful for prices like `"$1,299"` → `1299`.
    StripNonNumericInt,
    /// Remove all non-numeric characters (preserving `.`), then parse as float.
    StripNonNumericFloat,
}

impl FieldTransform {
    /// Parses a transform name from a TOML string.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "parse_int"              => Some(Self::ParseInt),
            "parse_float"            => Some(Self::ParseFloat),
            "parse_bool"             => Some(Self::ParseBool),
            "trim"                   => Some(Self::Trim),
            "lowercase"              => Some(Self::Lowercase),
            "uppercase"              => Some(Self::Uppercase),
            "strip_nonnumeric_int"   => Some(Self::StripNonNumericInt),
            "strip_nonnumeric_float" => Some(Self::StripNonNumericFloat),
            _ => None,
        }
    }

    /// Applies the transform to a raw extracted string, returning the typed
    /// [`FieldValue`]. Returns `FieldValue::Null` when parsing fails, and logs
    /// a warning — it never propagates an error, since a bad field value should
    /// not abort the entire extraction.
    fn apply(&self, raw: &str, field: &str) -> FieldValue {
        let trimmed = raw.trim();
        match self {
            Self::ParseInt => trimmed.parse::<i64>().map(FieldValue::Integer).unwrap_or_else(|_| {
                warn!(field, raw = trimmed, "parse_int failed; using Null");
                FieldValue::Null
            }),
            Self::ParseFloat => trimmed.parse::<f64>().map(FieldValue::Float).unwrap_or_else(|_| {
                warn!(field, raw = trimmed, "parse_float failed; using Null");
                FieldValue::Null
            }),
            Self::ParseBool => match trimmed.to_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => FieldValue::Bool(true),
                "false" | "0" | "no" | "off" => FieldValue::Bool(false),
                _ => {
                    warn!(field, raw = trimmed, "parse_bool failed; using Null");
                    FieldValue::Null
                }
            },
            Self::Trim => {
                FieldValue::Text(trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
            }
            Self::Lowercase => FieldValue::Text(trimmed.to_lowercase()),
            Self::Uppercase => FieldValue::Text(trimmed.to_uppercase()),
            Self::StripNonNumericInt => {
                let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
                digits.parse::<i64>().map(FieldValue::Integer).unwrap_or_else(|_| {
                    warn!(field, raw = trimmed, "strip_nonnumeric_int found no digits; using Null");
                    FieldValue::Null
                })
            }
            Self::StripNonNumericFloat => {
                let numeric: String = trimmed
                    .chars()
                    .filter(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                numeric.parse::<f64>().map(FieldValue::Float).unwrap_or_else(|_| {
                    warn!(field, raw = trimmed, "strip_nonnumeric_float found no digits; using Null");
                    FieldValue::Null
                })
            }
        }
    }
}

// ── CompiledExtractor ─────────────────────────────────────────────────────────

/// A single compiled extractor: schema metadata + pre-compiled selectors.
///
/// Compiling selectors once at load time means the hot extraction path does
/// only DOM traversal — no regex or selector compilation per page.
struct CompiledExtractor {
    name: String,
    url_pattern: Option<Regex>,
    fields: Vec<CompiledField>,
}

struct CompiledField {
    name: String,
    selector: Selector,
    attr: String,
    transform: Option<FieldTransform>,
    multiple: bool,
    optional: bool,
}

impl CompiledExtractor {
    /// Compiles an [`ExtractorSchema`] into a [`CompiledExtractor`].
    fn compile(schema: ExtractorSchema) -> Result<Self> {
        let url_pattern = schema
            .url_pattern
            .as_deref()
            .map(|p| {
                Regex::new(p).map_err(|e| {
                    CrawlError::Config(format!(
                        "invalid url_pattern in schema '{}': {e}",
                        schema.name
                    ))
                })
            })
            .transpose()?;

        let mut fields = Vec::with_capacity(schema.fields.len());

        for rule in schema.fields {
            let selector = Selector::parse(&rule.selector).map_err(|e| {
                CrawlError::Config(format!(
                    "invalid selector '{}' in schema '{}': {e:?}",
                    rule.selector, schema.name
                ))
            })?;

            let transform = rule
                .transform
                .as_deref()
                .map(|t| {
                    FieldTransform::from_name(t).ok_or_else(|| {
                        CrawlError::Config(format!(
                            "unknown transform '{}' in schema '{}'",
                            t, schema.name
                        ))
                    })
                })
                .transpose()?;

            fields.push(CompiledField {
                name: rule.name,
                selector,
                attr: rule.attr,
                transform,
                multiple: rule.multiple,
                optional: rule.optional,
            });
        }

        Ok(Self {
            name: schema.name,
            url_pattern,
            fields,
        })
    }

    /// Returns `true` if this extractor should run for the given response URL.
    fn matches_url(&self, url: &str) -> bool {
        match &self.url_pattern {
            Some(re) => re.is_match(url),
            None => true,
        }
    }

    /// Runs all field rules against the document and returns a populated record.
    fn extract(&self, response: &FetchResponse) -> Result<ExtractedRecord> {
        let html_str = response.text().map_err(|_| CrawlError::Extraction(
            "response body is not valid UTF-8".to_owned(),
        ))?;

        let document = Html::parse_document(html_str);
        let mut record = ExtractedRecord::new(&self.name, response.final_url.as_str());

        for field in &self.fields {
            let value = if field.multiple {
                self.extract_multiple(&document, field)
            } else {
                self.extract_single(&document, field)
            };

            match &value {
                FieldValue::Null if field.optional => {
                    // Skip optional fields that produced no match.
                    debug!(field = %field.name, "optional field has no match; omitting");
                    continue;
                }
                _ => {
                    record.insert(&field.name, value);
                }
            }
        }

        Ok(record)
    }

    /// Extracts the first matching element.
    fn extract_single(&self, document: &Html, field: &CompiledField) -> FieldValue {
        let raw = document
            .select(&field.selector)
            .next()
            .map(|el| Self::get_value(el, &field.attr));

        match raw {
            None => FieldValue::Null,
            Some(text) => Self::apply_transform(&text, field),
        }
    }

    /// Extracts all matching elements into a `FieldValue::List`.
    fn extract_multiple(&self, document: &Html, field: &CompiledField) -> FieldValue {
        let values: Vec<FieldValue> = document
            .select(&field.selector)
            .map(|el| {
                let raw = Self::get_value(el, &field.attr);
                Self::apply_transform(&raw, field)
            })
            .filter(|v| !v.is_null())
            .collect();

        if values.is_empty() {
            FieldValue::Null
        } else {
            FieldValue::List(values)
        }
    }

    /// Extracts the value from an element: inner text or a named attribute.
    fn get_value(element: scraper::ElementRef, attr: &str) -> String {
        if attr == "text" {
            element.text().collect::<String>().trim().to_owned()
        } else {
            element
                .value()
                .attr(attr)
                .unwrap_or("")
                .trim()
                .to_owned()
        }
    }

    /// Applies the field's transform, or returns a plain `Text` if none.
    fn apply_transform(raw: &str, field: &CompiledField) -> FieldValue {
        if raw.is_empty() {
            return FieldValue::Null;
        }
        match &field.transform {
            Some(t) => t.apply(raw, &field.name),
            None => FieldValue::Text(raw.trim().to_owned()),
        }
    }
}

// ── SchemaExtractor ───────────────────────────────────────────────────────────

/// A single compiled schema extractor, implementing the [`Extractor`] trait.
///
/// Wrap in `Arc` and register with a [`SchemaRegistry`].
pub struct SchemaExtractor {
    inner: Arc<CompiledExtractor>,
}

impl SchemaExtractor {
    /// Loads and compiles a schema from a TOML string.
    ///
    /// Returns a `Vec` because a single TOML file may define multiple
    /// `[[extractor]]` entries.
    pub fn from_toml(toml_str: &str) -> Result<Vec<Self>> {
        let file: SchemaFile = toml::from_str(toml_str)
            .map_err(|e| CrawlError::Config(format!("invalid schema TOML: {e}")))?;

        file.extractor
            .into_iter()
            .map(|schema| {
                CompiledExtractor::compile(schema)
                    .map(|c| Self { inner: Arc::new(c) })
            })
            .collect()
    }
}

#[async_trait]
impl Extractor for SchemaExtractor {
    fn matches(&self, response: &FetchResponse) -> bool {
        response.is_html() && self.inner.matches_url(response.url.as_str())
    }

    #[instrument(skip(self, response), fields(schema = %self.inner.name, url = %response.url))]
    async fn extract(&self, response: &FetchResponse) -> Result<ExtractedRecord> {
        // Extraction is CPU-bound. We run it on the blocking thread pool to
        // avoid stalling the Tokio I/O reactor.
        let inner = Arc::clone(&self.inner);
        let url = response.url.clone();
        let final_url = response.final_url.clone();
        let body = response.body.clone();
        let headers = response.headers.clone();
        let status = response.status;
        let fetch_duration = response.fetch_duration;

        tokio::task::spawn_blocking(move || {
            let synthetic_response = FetchResponse {
                url,
                final_url,
                status,
                headers,
                body,
                fetch_duration,
            };
            inner.extract(&synthetic_response)
        })
            .await
            .map_err(|e| CrawlError::Extraction(format!("extractor task panicked: {e}")))?
    }

    fn schema_name(&self) -> &str {
        &self.inner.name
    }
}

// ── SchemaRegistry ────────────────────────────────────────────────────────────

/// Holds all active schema extractors and dispatches responses to matching ones.
///
/// Construct with [`SchemaRegistry::from_dir`] to load every `*.toml` in a
/// directory, or with [`SchemaRegistry::from_toml`] for testing.
pub struct SchemaRegistry {
    extractors: Vec<SchemaExtractor>,
}

impl SchemaRegistry {
    /// Loads all `*.toml` schema files from `dir`.
    ///
    /// Files that fail to parse are logged as warnings and skipped — a bad
    /// schema file should not abort startup.
    pub async fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let mut extractors = Vec::new();

        let mut entries = tokio::fs::read_dir(dir)
            .await
            .map_err(|e| CrawlError::Config(format!("cannot read schema dir {:?}: {e}", dir)))?;

        while let Some(entry) = entries.next_entry().await.map_err(|e| {
            CrawlError::Config(format!("error reading schema dir entry: {e}"))
        })? {
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }

            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = ?path, error = %e, "failed to read schema file; skipping");
                    continue;
                }
            };

            match SchemaExtractor::from_toml(&content) {
                Ok(schemas) => {
                    debug!(path = ?path, count = schemas.len(), "loaded schema file");
                    extractors.extend(schemas);
                }
                Err(e) => {
                    warn!(path = ?path, error = %e, "failed to parse schema file; skipping");
                }
            }
        }

        Ok(Self { extractors })
    }

    /// Constructs a registry from a raw TOML string. Primarily for testing.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let extractors = SchemaExtractor::from_toml(toml_str)?;
        Ok(Self { extractors })
    }

    /// Runs every matching extractor against `response` and returns all records.
    ///
    /// If multiple schemas match (e.g. a page is both an "article" and a "product"),
    /// all matching records are returned. Individual extractor errors are logged
    /// as warnings and skipped — one bad schema should not suppress others.
    pub async fn extract_all(&self, response: &FetchResponse) -> Vec<ExtractedRecord> {
        let mut records = Vec::new();

        for extractor in &self.extractors {
            if !extractor.matches(response) {
                continue;
            }

            match extractor.extract(response).await {
                Ok(record) => records.push(record),
                Err(e) => {
                    warn!(
                        schema = extractor.schema_name(),
                        url = %response.url,
                        error = %e,
                        "extraction failed; skipping schema"
                    );
                }
            }
        }

        records
    }

    /// Number of registered extractors.
    pub fn len(&self) -> usize {
        self.extractors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.extractors.is_empty()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::{collections::HashMap, time::Duration};

    fn html_response(url: &str, body: &str) -> FetchResponse {
        FetchResponse {
            url: url.parse().unwrap(),
            final_url: url.parse().unwrap(),
            status: 200,
            headers: {
                let mut h = HashMap::new();
                h.insert("content-type".to_owned(), "text/html".to_owned());
                h
            },
            body: Bytes::from(body.to_owned()),
            fetch_duration: Duration::ZERO,
        }
    }

    const SCHEMA: &str = r#"
[[extractor]]
name = "article"
url_pattern = "example\\.com"

[[extractor.fields]]
name     = "title"
selector = "h1"
attr     = "text"

[[extractor.fields]]
name      = "price"
selector  = "span.price"
attr      = "text"
transform = "strip_nonnumeric_float"

[[extractor.fields]]
name     = "tags"
selector = "span.tag"
attr     = "text"
multiple = true

[[extractor.fields]]
name     = "missing"
selector = "div.nope"
attr     = "text"
optional = true
"#;

    #[tokio::test]
    async fn extracts_text_fields() {
        let registry = SchemaRegistry::from_toml(SCHEMA).unwrap();
        let response = html_response(
            "https://example.com/article",
            r#"<html><body>
                <h1>Hello World</h1>
                <span class="price">$1,299.99</span>
                <span class="tag">rust</span>
                <span class="tag">async</span>
            </body></html>"#,
        );

        let records = registry.extract_all(&response).await;
        assert_eq!(records.len(), 1);

        let rec = &records[0];
        assert_eq!(rec.schema, "article");
        assert_eq!(rec.get("title"), Some(&FieldValue::Text("Hello World".to_owned())));
        assert_eq!(rec.get("price"), Some(&FieldValue::Float(1299.99)));
        assert_eq!(
            rec.get("tags"),
            Some(&FieldValue::List(vec![
                FieldValue::Text("rust".to_owned()),
                FieldValue::Text("async".to_owned()),
            ]))
        );
        // Optional field with no match should be absent entirely.
        assert!(rec.get("missing").is_none());
    }

    #[tokio::test]
    async fn url_pattern_filters_non_matching_urls() {
        let registry = SchemaRegistry::from_toml(SCHEMA).unwrap();
        let response = html_response(
            "https://other.com/page",
            "<html><body><h1>Ignored</h1></body></html>",
        );

        let records = registry.extract_all(&response).await;
        assert!(records.is_empty(), "non-matching URL should produce no records");
    }

    #[test]
    fn field_transform_parse_int() {
        assert_eq!(
            FieldTransform::ParseInt.apply("42", "f"),
            FieldValue::Integer(42)
        );
        assert_eq!(
            FieldTransform::ParseInt.apply("not a number", "f"),
            FieldValue::Null
        );
    }

    #[test]
    fn field_transform_strip_nonnumeric_float() {
        assert_eq!(
            FieldTransform::StripNonNumericFloat.apply("$1,299.99", "price"),
            FieldValue::Float(1299.99)
        );
    }

    #[test]
    fn schema_extractor_rejects_bad_toml() {
        let result = SchemaExtractor::from_toml("not valid toml ][[[");
        assert!(result.is_err());
    }
}