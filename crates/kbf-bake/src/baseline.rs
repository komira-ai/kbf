//! The baseline the error and latency gates compare against, taken at deploy start
//! (`docs/design/kbf-cicd.md`, section 7).
//!
//! The farm keeps no time series. The error ratio is read from the invocation records of the
//! seven days before the deploy (`GET /v1/invocations?since&until`): failed actions over
//! executed ones, across every closed invocation. That is the records' own measure, which is
//! not the `kbf_execute_errors_total` ratio the gate holds the bake to; it is the closest
//! the records give. The latency baseline is the p99 of the evaluator's own earlier scrapes
//! ([`crate::ledger`]). A baseline that cannot be had is an error with its reason, and the
//! file records it as absent, which leaves the gate red.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{WriteError, write_json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::ledger::Ledger;
use crate::table::BASELINE_SECS;

/// The baseline file's format number.
pub const BASELINE_SCHEMA: u32 = 1;
/// The most pages of invocations one baseline reads; more is no baseline.
pub const MAX_PAGES: usize = 10_000;
/// Invocations asked of one page.
const PAGE_SIZE: usize = 1000;

/// What `kbf-bake baseline` writes and `kbf-bake run --baseline` reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineDoc {
    /// [`BASELINE_SCHEMA`].
    pub schema: u32,
    /// The deploy start the baseline is for (unix seconds).
    pub taken_at: u64,
    /// Failed over executed actions in the seven days before; absent when unavailable.
    pub error_ratio: Option<f64>,
    /// Queue-to-start p99 in seconds; absent when unavailable.
    pub queue_p99: Option<f64>,
    /// Why each absent value is absent.
    pub missing: Vec<String>,
}

impl BaselineDoc {
    /// Whether both values are there.
    pub fn complete(&self) -> bool {
        self.error_ratio.is_some() && self.queue_p99.is_some()
    }

    /// Read a baseline file; a value that is not usable is refused.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let doc: Self = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} is not a baseline: {e}", path.display()))?;
        if doc.schema != BASELINE_SCHEMA {
            return Err(format!(
                "{}: schema {}, this reader knows {BASELINE_SCHEMA}",
                path.display(),
                doc.schema
            ));
        }
        let usable = |v: Option<f64>, least: f64| v.is_none_or(|v| v >= least);
        if !usable(doc.error_ratio, 0.0) || !usable(doc.queue_p99, f64::MIN_POSITIVE) {
            return Err(format!(
                "{} holds a baseline that is not usable",
                path.display()
            ));
        }
        Ok(doc)
    }

    /// Write the file, atomically.
    pub fn save(&self, path: &Path) -> Result<(), WriteError> {
        write_json(path, self)
    }
}

/// `unix` seconds as the RFC 3339 time the invocation list asks for.
pub fn rfc3339(unix: u64) -> Result<String, String> {
    i64::try_from(unix)
        .ok()
        .and_then(|secs| OffsetDateTime::from_unix_timestamp(secs).ok())
        .and_then(|t| t.format(&Rfc3339).ok())
        .ok_or_else(|| format!("{unix} is not a time"))
}

#[derive(Deserialize)]
struct Page {
    invocations: Vec<Item>,
    next_page: Option<String>,
    #[serde(default)]
    partial: bool,
}

#[derive(Deserialize)]
struct Item {
    counts: Counts,
}

#[derive(Deserialize)]
struct Counts {
    ran: u64,
    failed: u64,
}

/// Failed over executed actions across the closed invocations of the seven days before `at`,
/// read through `get` (a GET of a URL, the body or why not) from the API at `origin`. No
/// executed action, an answer that is `partial`, a page that is not the list, or more than
/// [`MAX_PAGES`] pages, is an error.
pub fn error_ratio(
    get: &mut dyn FnMut(&str) -> Result<String, String>,
    origin: &str,
    at: u64,
) -> Result<f64, String> {
    let origin = origin.trim_end_matches('/');
    let since = rfc3339(at.saturating_sub(BASELINE_SECS))?;
    let until = rfc3339(at)?;
    let mut url = format!(
        "{origin}/v1/invocations?since={since}&until={until}&state=closed&page_size={PAGE_SIZE}"
    );
    let (mut ran, mut failed) = (0u64, 0u64);
    for _ in 0..MAX_PAGES {
        let body = get(&url)?;
        let page: Page = serde_json::from_str(&body)
            .map_err(|e| format!("{url}: not an invocation list: {e}"))?;
        if page.partial {
            return Err(format!(
                "{url}: the list is partial (an instance could not be read)"
            ));
        }
        for item in &page.invocations {
            ran = ran.saturating_add(item.counts.ran);
            failed = failed.saturating_add(item.counts.failed);
        }
        match page.next_page {
            None => {
                if ran == 0 {
                    return Err(
                        "no action was executed in the seven days before the deploy".to_owned()
                    );
                }
                return Ok(failed as f64 / ran as f64);
            }
            // The link is a path on the same API; anything else is not followed.
            Some(next) if next.starts_with('/') => url = format!("{origin}{next}"),
            Some(next) => return Err(format!("the next page `{next}` is not a path")),
        }
    }
    Err(format!("more than {MAX_PAGES} pages of invocations"))
}

/// Take the baseline for a deploy that starts at `at`.
pub fn take(
    get: &mut dyn FnMut(&str) -> Result<String, String>,
    origin: &str,
    ledger: &Ledger,
    at: u64,
) -> BaselineDoc {
    let mut missing = Vec::new();
    let error_ratio = error_ratio(get, origin, at)
        .map_err(|why| missing.push(format!("error ratio: {why}")))
        .ok();
    let queue_p99 = ledger
        .queue_p99(at)
        .map_err(|why| missing.push(format!("queue p99: {why}")))
        .ok();
    BaselineDoc {
        schema: BASELINE_SCHEMA,
        taken_at: at,
        error_ratio,
        queue_p99,
        missing,
    }
}
