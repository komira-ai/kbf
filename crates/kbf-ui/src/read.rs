//! What a page reads, and the contract that it reads nothing else.
//!
//! Every page is a [`Page`]: its name and the read API routes it may read, as route
//! patterns (`/v1/invocations/{id}/actions`). A page reads only through a
//! [`Reader`] made for it, which refuses a URL that matches none of its routes (or
//! holds a dot segment or a backslash, which a URL client would resolve to another
//! route) before anything is sent, and records every URL it was asked for, refused or not, so the
//! page can show them. The contract test (`tests.rs`) renders every page and checks
//! both what each page asked for and what reached the source.

use serde::de::DeserializeOwned;

use crate::html::refused_in_url;
use crate::{Fetched, Source};

/// A page and the routes it may read.
#[derive(Debug)]
pub(crate) struct Page {
    /// The page's name, as its `<title>` and navigation show it.
    pub(crate) name: &'static str,
    /// The read API routes the page may read, as patterns: a `{name}` segment
    /// matches any one non-empty segment but `.` or `..` (in any spelling); a path
    /// with a backslash, whitespace or a control character matches none; the query
    /// is not part of the match.
    pub(crate) reads: &'static [&'static str],
}

impl Page {
    /// Whether `url` (a path, perhaps with a query) is one of the page's routes.
    pub(crate) fn declares(&self, url: &str) -> bool {
        let path = url.split_once('?').map_or(url, |(path, _)| path);
        self.reads.iter().any(|pattern| matches(pattern, path))
    }
}

/// Whether `path` matches route `pattern` segment by segment. A dot segment never
/// matches: a URL client resolves it, so the URL sent would not be the route matched.
/// Nor does a path with a character [`refused_in_url`]: a client reads a backslash
/// as `/`, so its segments are not the ones matched here.
fn matches(pattern: &str, path: &str) -> bool {
    if path.chars().any(refused_in_url) {
        return false;
    }
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (_, Some(g)) if is_dot_segment(g) => return false,
            (Some(w), Some(g)) if w.starts_with('{') && w.ends_with('}') && !g.is_empty() => {}
            (Some(w), Some(g)) if w == g => {}
            _ => return false,
        }
    }
}

/// Whether `segment` (percent-encoded) is `.` or `..` in any spelling a URL client
/// resolves: `%2e` in either case for a dot (WHATWG URL, "single-dot" and
/// "double-dot" segments).
fn is_dot_segment(segment: &str) -> bool {
    let dots = segment.to_ascii_lowercase().replace("%2e", ".");
    dots == "." || dots == ".."
}

/// Why a read gave the page no document.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadError {
    /// The URL is none of the page's routes: a page bug, refused before any request.
    #[error("{url} is not a route this page declares")]
    Undeclared { url: String },
    /// The source could not be asked.
    #[error("{url}: {why}")]
    Unreachable { url: String, why: String },
    /// The API answered with another status than 200.
    #[error("{url} answered {status}: {message}")]
    Status {
        url: String,
        status: u16,
        message: String,
        /// The answer's headers that a page with its status passes on.
        headers: Vec<(&'static str, String)>,
    },
    /// The answer is not the document the page expects.
    #[error("{url} answered a document this page cannot read: {why}")]
    Malformed { url: String, why: String },
}

impl ReadError {
    /// The status of a page whose read failed so: the API's own when it answered one
    /// (a build it has no record of is `404` on the page too), `502` when it could not
    /// be read or answered a document the page cannot read, `500` for a page bug.
    pub(crate) const fn status(&self) -> u16 {
        match self {
            Self::Status { status, .. } => *status,
            Self::Undeclared { .. } => 500,
            Self::Unreachable { .. } | Self::Malformed { .. } => 502,
        }
    }

    /// The headers a page answering [`Self::status`] passes on: the API's
    /// `WWW-Authenticate` and `Retry-After`, when it answered a status.
    pub(crate) fn headers(&self) -> &[(&'static str, String)] {
        match self {
            Self::Status { headers, .. } => headers,
            _ => &[],
        }
    }

    /// Whether the fault is the page's own (an undeclared read), not the API's.
    pub(crate) const fn is_page_bug(&self) -> bool {
        matches!(self, Self::Undeclared { .. })
    }

    /// Whether the read was refused because the farm keeps no history: a `404` from
    /// one of the routes that exist only with `--history-objects` (`docs/api.md`,
    /// "Across builds": otherwise `404` as any unknown path), with the empty body of
    /// an unknown route (or the words `history is off`). A record the history does not
    /// hold is a `404` with its own code (`no_record`, `no_shape`, `no_run`, `no_log`),
    /// and the routes being off by policy a `404` with `off`; neither is this.
    pub(crate) fn is_history_off(&self) -> bool {
        match self {
            Self::Status {
                url,
                status: 404,
                message,
                ..
            } => {
                is_history_route(url)
                    && (message.is_empty() || message.starts_with("history is off"))
            }
            _ => false,
        }
    }
}

/// Whether `url` is a route that exists only when the farm keeps history: the list of
/// builds, the shapes, tests and grid, and the runs (with their logs).
fn is_history_route(url: &str) -> bool {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let under = |root: &str| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    ["/v1/shapes", "/v1/tests", "/v1/grid", "/v1/runs"]
        .into_iter()
        .any(under)
        || path == "/v1/invocations"
}

/// A ranged read: the bytes the API sent, and the size of the whole when it said.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Slice {
    pub(crate) bytes: Vec<u8>,
    /// The whole object's size: from `Content-Range` of a `206` or `416`, or the body's
    /// length when the API ignored the range and sent it all.
    pub(crate) total: Option<u64>,
}

/// The size after the `/` of a `Content-Range` value (`bytes 0-9/100`, `bytes */0`).
fn content_range_total(headers: &[(String, String)]) -> Option<u64> {
    let (_, value) = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-range"))?;
    value.rsplit_once('/')?.1.trim().parse().ok()
}

/// A URL a page asked for, and whether it was sent (a refused one was not).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Asked {
    pub(crate) url: String,
    pub(crate) sent: bool,
}

/// Reads for one page, through its contract.
pub(crate) struct Reader<'a, S> {
    source: &'a S,
    page: &'a Page,
    asked: Vec<Asked>,
}

impl<'a, S: Source> Reader<'a, S> {
    pub(crate) const fn new(source: &'a S, page: &'a Page) -> Self {
        Self {
            source,
            page,
            asked: Vec::new(),
        }
    }

    /// Every URL the page asked for, in order, refused ones included.
    pub(crate) fn asked(&self) -> &[Asked] {
        &self.asked
    }

    /// The JSON document at `url`.
    pub(crate) async fn json<T: DeserializeOwned>(&mut self, url: &str) -> Result<T, ReadError> {
        let body = self.ok(url).await?;
        serde_json::from_slice(&body).map_err(|e| ReadError::Malformed {
            url: url.to_owned(),
            why: e.to_string(),
        })
    }

    /// The text at `url`, with bytes that are not UTF-8 replaced.
    pub(crate) async fn text(&mut self, url: &str) -> Result<String, ReadError> {
        let body = self.ok(url).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Bytes `first` to `last` (inclusive) of the object at `url`, read with a `Range`
    /// header, so no more than that is read. The answer is `206` (the size is the
    /// `Content-Range`'s), `416` for a start past the end (an empty object: no bytes,
    /// the size from `Content-Range`), or `200` when the API ignored the range (the
    /// whole body, which is the size).
    pub(crate) async fn range(
        &mut self,
        url: &str,
        first: u64,
        last: u64,
    ) -> Result<Slice, ReadError> {
        let got = self.send(url, Some((first, last))).await?;
        let total = match got.status {
            200 => Some(got.body.len() as u64),
            206 | 416 => content_range_total(&got.headers),
            _ => return Err(refused(url, got)),
        };
        let bytes = if got.status == 416 {
            Vec::new()
        } else {
            got.body
        };
        Ok(Slice { bytes, total })
    }

    /// The body of a `200` answer at `url`.
    async fn ok(&mut self, url: &str) -> Result<Vec<u8>, ReadError> {
        let got = self.send(url, None).await?;
        if got.status == 200 {
            return Ok(got.body);
        }
        Err(refused(url, got))
    }

    /// The answer at `url`, whatever its status: refused before any request when the
    /// page does not declare `url`.
    async fn send(&mut self, url: &str, range: Option<(u64, u64)>) -> Result<Fetched, ReadError> {
        let sent = self.page.declares(url);
        self.asked.push(Asked {
            url: url.to_owned(),
            sent,
        });
        if !sent {
            return Err(ReadError::Undeclared {
                url: url.to_owned(),
            });
        }
        let asking = match range {
            None => self.source.get(url).await,
            Some((first, last)) => self.source.get_range(url, first, last).await,
        };
        asking.map_err(|e| ReadError::Unreachable {
            url: url.to_owned(),
            why: e.to_string(),
        })
    }
}

/// The error of an answer that is not the one the page wanted.
fn refused(url: &str, got: Fetched) -> ReadError {
    ReadError::Status {
        url: url.to_owned(),
        status: got.status,
        message: error_message(&got.body),
        headers: passed_on(got.headers),
    }
}

/// The API headers a page that answers with the API's status passes on, by the
/// names a page answer gives them: the ones that status depends on (RFC 9110 15.5.2
/// requires a `401` to carry `WWW-Authenticate`; `Retry-After` tells a client of a
/// `429` or `503` when to ask again).
const PASSED_ON: [&str; 2] = ["www-authenticate", "retry-after"];

/// The headers of `headers` in [`PASSED_ON`], in their order.
fn passed_on(headers: Vec<(String, String)>) -> Vec<(&'static str, String)> {
    headers
        .into_iter()
        .filter_map(|(name, value)| {
            let name = PASSED_ON
                .into_iter()
                .find(|p| p.eq_ignore_ascii_case(&name))?;
            Some((name, value))
        })
        .collect()
}

/// The `error` string of an API error body, with the `records_since` of a `no_record`
/// answer (`docs/design/invocations-api.md` 4.10), or the body as text when it has no
/// `error`.
fn error_message(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return String::from_utf8_lossy(body).into_owned();
    };
    let Some(error) = v.get("error").and_then(serde_json::Value::as_str) else {
        return String::from_utf8_lossy(body).into_owned();
    };
    match v.get("records_since").and_then(serde_json::Value::as_str) {
        Some(since) => format!("{error}: no record of it is held; records are held since {since}"),
        None => error.to_owned(),
    }
}

#[cfg(test)]
mod tests;
