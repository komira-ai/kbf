//! When the server started: the note on the Overview page, from the start time that
//! `GET /v1/health` carries (`started_unix_ms`, `uptime_ms`; `docs/api.md`).
//!
//! The banner is a note, never a precondition: a server that sends no start time (an
//! older one) still gets a page, and the banner says that the start time is not known and
//! why, never a guessed one.

use crate::html::{Html, attr};

/// The route that carries the start time.
const HEALTH: &str = "/v1/health";

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

/// `ms` milliseconds as the two largest units that matter: `850 ms`, `41.2 s`,
/// `3 min 20 s`, `2 h 5 min`, `4 d 3 h`. Truncated, never rounded up into the next unit.
fn age(ms: u64) -> String {
    const SECOND: u64 = 1000;
    const MINUTE: u64 = 60 * SECOND;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if ms < SECOND {
        format!("{ms} ms")
    } else if ms < MINUTE {
        format!("{}.{} s", ms / SECOND, (ms % SECOND) / 100)
    } else if ms < HOUR {
        format!("{} min {} s", ms / MINUTE, (ms % MINUTE) / SECOND)
    } else if ms < DAY {
        format!("{} h {} min", ms / HOUR, (ms % HOUR) / MINUTE)
    } else {
        format!("{} d {} h", ms / DAY, (ms % DAY) / HOUR)
    }
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
