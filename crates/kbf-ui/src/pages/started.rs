//! When the server started: the second half of the restart banner of the Nodes, Node,
//! Overview, Leases and Queue pages. What those pages show is held in the server's memory
//! and fills as daemons reconnect, so "is this empty because of an outage, or because the
//! server started a minute ago" is answered by the start time, which `GET /v1/health`
//! carries (`started_unix_ms`, `uptime_ms`; `docs/api.md`).
//!
//! The banner is a note, never a precondition: a page whose health read fails, or whose
//! server sends no start time (an older one), still renders, and the banner says that the
//! start time is not known and why, never a guessed one.

use serde::Deserialize;

use super::sched::age;
use crate::Source;
use crate::html::{Html, attr};
use crate::read::Reader;

/// The route that carries the start time.
pub(crate) const HEALTH: &str = "/v1/health";

/// `GET /v1/health`: the two keys the banner shows.
#[derive(Debug, Deserialize)]
struct Run {
    #[serde(default)]
    started_unix_ms: Option<u64>,
    #[serde(default)]
    uptime_ms: Option<u64>,
}

/// What the banner can say about the server's start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Started {
    /// The start, as the server gave it, and how long ago by its own clock when it said.
    At {
        unix_ms: u64,
        uptime_ms: Option<u64>,
    },
    /// No start time, and why.
    Unsaid(String),
}

impl Started {
    /// The start from the two keys of a health document.
    pub(crate) fn of(unix_ms: Option<u64>, uptime_ms: Option<u64>) -> Self {
        match unix_ms {
            Some(unix_ms) => Self::At { unix_ms, uptime_ms },
            None => Self::Unsaid(format!("{HEALTH} does not say")),
        }
    }
}

/// The start time, read through `reader`, whose page declares [`HEALTH`] among its routes.
/// A read that fails is [`Started::Unsaid`], never a failure of the page.
pub(crate) async fn read<S: Source>(reader: &mut Reader<'_, S>) -> Started {
    match reader.json::<Run>(HEALTH).await {
        Ok(run) => Started::of(run.started_unix_ms, run.uptime_ms),
        Err(e) => Started::Unsaid(e.to_string()),
    }
}

/// The banner's note: when the server started.
pub(crate) fn note(h: &mut Html, started: &Started) {
    h.el("p", &[attr("class", "hint")], |h| match started {
        Started::At { unix_ms, uptime_ms } => {
            h.text(&format!("This server started at {}", rfc3339(*unix_ms)));
            match uptime_ms {
                Some(up) => h.text(&format!(", {} ago by its own clock.", age(*up))),
                None => h.text("."),
            };
        }
        Started::Unsaid(why) => {
            h.text(&format!("When this server started is not known: {why}."));
        }
    });
}

/// `ms` since the Unix epoch as an RFC 3339 UTC time.
fn rfc3339(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (proleptic Gregorian; Hinnant's algorithm).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests;
