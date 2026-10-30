//! The v2 key set: the presigned PUT URLs the frontend issues for one run.
//!
//! The set is fixed before the run starts: `packs`, the URLs of the pack objects (each a
//! `kbf-segments` pack of at most [`MAX_PACK_BYTES`](kbf_task_pack::MAX_PACK_BYTES) bytes; the
//! first holds the manifest at offset 0 and is the commit point), and `large`, the URLs of the
//! large objects (one raw blob each, for a blob over the record cap). It is given as JSON,
//!
//! ```json
//! {"packs":["https://store/a?sig","https://store/b?sig"],"large":["https://store/c?sig"]}
//! ```
//!
//! from `--put-urls-file` (read, never deleted) or, as a stopgap that needs
//! `--insecure-argv-secrets` like the other credentials in argv, from `--put-urls`.
//!
//! The file's protection is the executor's per-run directory, not its mode: the executor delivers
//! it read-only for the run, so the tool does not check its permissions.
//!
//! The URLs are credentials, so no error here contains any part of the input: a refusal names
//! the field and the rule, and a JSON syntax error only its line and column.
//!
//! # How the run uses it
//!
//! Slot `i` of `packs` is where pack `i` is PUT (pack 0 holds the manifest and is the commit
//! point), and slot `i` of `large` is where large object `i` is PUT. The run needs at most as
//! many of each as its outputs call for ([`crate::plan`]); URLs it does not need are not used.
//! The lengths of the lists are `N` and `M` for the plan; a list longer than a manifest can name (8) is refused at startup (exit 14). The v1 flag
//! `--pack-url-file` is a key set of one pack and no large object.

use std::fmt;
use std::path::Path;

use kbf_task_pack::{MAX_LARGE, MAX_PACKS};
use serde_json::Value;

/// The only keys the key set's JSON may hold.
const FIELDS: [&str; 2] = ["packs", "large"];

/// The PUT URLs of one run.
#[derive(Clone, PartialEq, Eq)]
pub struct PutUrls {
    /// The pack URLs, slot 0 first. Never empty.
    pub packs: Vec<String>,
    /// The large-object URLs, slot 0 first. May be empty.
    pub large: Vec<String>,
}

// The URLs are credentials: a `Debug` that printed them would put them in a log.
impl fmt::Debug for PutUrls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PutUrls")
            .field("packs", &self.packs.len())
            .field("large", &self.large.len())
            .finish()
    }
}

/// Why a key set was refused. No variant holds a URL or any part of the input.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PutUrlsError {
    /// The file could not be read.
    #[error("{path}: {reason}")]
    Read {
        /// The file.
        path: String,
        /// The operating system's reason.
        reason: String,
    },
    /// The text is not JSON.
    #[error("the key set is not JSON (line {line}, column {column})")]
    Syntax {
        /// Where the parser stopped.
        line: usize,
        /// Where the parser stopped.
        column: usize,
    },
    /// The JSON is not an object.
    #[error("the key set is not a JSON object")]
    NotAnObject,
    /// The object holds a key other than `packs` and `large`.
    #[error("the key set holds a field other than `packs` and `large`")]
    UnknownField,
    /// `packs` is absent.
    #[error("the key set has no `{field}` field")]
    Missing {
        /// The field.
        field: &'static str,
    },
    /// A field is not an array of strings.
    #[error("`{field}` is not an array of strings")]
    NotStrings {
        /// The field.
        field: &'static str,
    },
    /// `packs` is empty: pack 0 is the commit point and must have a URL.
    #[error("`packs` is empty; pack 0 needs a URL")]
    NoPacks,
    /// A list has more URLs than a manifest can name: [`MAX_PACKS`] packs, [`MAX_LARGE`] large
    /// objects.
    #[error("`{field}` has {count} URLs, more than the {max} a run can use")]
    TooMany {
        /// The field.
        field: &'static str,
        /// How many URLs.
        count: usize,
        /// The most it may hold.
        max: usize,
    },
    /// A URL is empty, is not an `http` or `https` URL, or holds whitespace or a control
    /// character.
    #[error("`{field}` slot {slot} is not an http(s) URL")]
    BadUrl {
        /// The field.
        field: &'static str,
        /// The index in the list.
        slot: usize,
    },
}

impl PutUrls {
    /// Parses the JSON key set.
    ///
    /// # Errors
    /// A [`PutUrlsError`] for text that is not a JSON object of the fields `packs` (required, a
    /// non-empty array) and `large` (optional, an array; absent means none), each of at most
    /// [`MAX_PACKS`] or [`MAX_LARGE`] `http` or `https` URLs. Any other field is refused, so a misspelt `large`
    /// is not read as "no large objects".
    pub fn parse(text: &str) -> Result<Self, PutUrlsError> {
        let value: Value = serde_json::from_str(text).map_err(|e| PutUrlsError::Syntax {
            line: e.line(),
            column: e.column(),
        })?;
        let Value::Object(mut object) = value else {
            return Err(PutUrlsError::NotAnObject);
        };
        if object.keys().any(|k| !FIELDS.contains(&k.as_str())) {
            return Err(PutUrlsError::UnknownField);
        }
        let packs = urls(object.remove("packs"), "packs", usize::from(MAX_PACKS))?
            .ok_or(PutUrlsError::Missing { field: "packs" })?;
        let large =
            urls(object.remove("large"), "large", usize::from(MAX_LARGE))?.unwrap_or_default();
        if packs.is_empty() {
            return Err(PutUrlsError::NoPacks);
        }
        Ok(Self { packs, large })
    }

    /// The key set the flags name: `file` (read, left in place), else the `flag` value, else
    /// `None` when neither is given.
    ///
    /// # Errors
    /// A [`PutUrlsError`] for a file that cannot be read or text that [`PutUrls::parse`]
    /// refuses.
    pub fn from_sources(
        file: Option<&Path>,
        flag: Option<&str>,
    ) -> Result<Option<Self>, PutUrlsError> {
        match (file, flag) {
            (Some(path), _) => {
                let text = std::fs::read_to_string(path).map_err(|e| PutUrlsError::Read {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })?;
                Self::parse(&text).map(Some)
            }
            (None, Some(text)) => Self::parse(text).map(Some),
            (None, None) => Ok(None),
        }
    }
}

/// The URLs of the list `value` holds under `field`; `None` when the field is absent.
fn urls(
    value: Option<Value>,
    field: &'static str,
    max: usize,
) -> Result<Option<Vec<String>>, PutUrlsError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Value::Array(items) = value else {
        return Err(PutUrlsError::NotStrings { field });
    };
    if items.len() > max {
        return Err(PutUrlsError::TooMany {
            field,
            count: items.len(),
            max,
        });
    }
    let mut urls = Vec::with_capacity(items.len());
    for (slot, item) in items.into_iter().enumerate() {
        let Value::String(url) = item else {
            return Err(PutUrlsError::NotStrings { field });
        };
        if !is_http_url(&url) {
            return Err(PutUrlsError::BadUrl { field, slot });
        }
        urls.push(url);
    }
    Ok(Some(urls))
}

/// Whether `url` is `http://` or `https://` (any case) followed by something, with no
/// whitespace or control character.
fn is_http_url(url: &str) -> bool {
    let scheme = ["https://", "http://"].into_iter().find(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    });
    let Some(scheme) = scheme else {
        return false;
    };
    url.len() > scheme.len() && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
#[path = "put_urls_tests.rs"]
mod tests;
