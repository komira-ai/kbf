//! The JSON object a step writes as the last line of its standard output.
//!
//! ```json
//! {"class":"release","reason":"...","retry_after_secs":300,"reversible":true,
//!  "promotion_pending":false,"detail":"..."}
//! ```
//!
//! The updater reads the last line leniently ([`Report::read`]): a missing or malformed line is
//! `{}`, which is a `release` failure class, a step that can still be undone and nothing pending.
//! The conformance harness reads it strictly ([`Report::from_stdout`]) and refuses a hook that
//! leaves it out.

use std::io::{self, Write};

use serde::Deserialize;
use serde_json::{Map, Value};

/// Whom a failure is blamed on (it decides whether the fleet rolls back).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// The new version is wrong. The default, and what a step that dies without the line is.
    #[default]
    Release,
    /// Something remote or shared failed (an unreachable server); the release may be fine.
    Environment,
}

impl Class {
    /// The word in the JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Class::Release => "release",
            Class::Environment => "environment",
        }
    }
}

/// Why the last line was not read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LineError {
    #[error("the step wrote no last line")]
    Missing,
    #[error("the last line is not the report object: {0}")]
    Malformed(String),
    #[error("reason {0:?} is not a token of [a-z0-9_], 1 to {MAX_REASON_LEN} bytes")]
    BadReason(String),
}

/// The longest `reason` token.
pub const MAX_REASON_LEN: usize = 64;

/// Whether `reason` is a machine-readable token: lowercase letters, digits and `_`, at most
/// [`MAX_REASON_LEN`] bytes, not empty (`state_invalid`, `drain_bound`).
pub fn is_reason_token(reason: &str) -> bool {
    (1..=MAX_REASON_LEN).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// What a step tells the updater besides its exit code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub class: Class,
    /// A short machine-readable token (see [`is_reason_token`]); prose goes in `detail`.
    pub reason: Option<String>,
    /// Used with exit `75`: when the updater may order the host again.
    pub retry_after_secs: Option<u64>,
    /// Whether the hook can still undo what it has done. A step that says `false` is the commit.
    /// Absent means `true`.
    pub reversible: bool,
    /// Set by `verify`: serving, but not yet fully its own.
    pub promotion_pending: bool,
    /// A short text for the log and the alert.
    pub detail: Option<String>,
}

impl Default for Report {
    /// The report of a step that wrote nothing: `{}`.
    fn default() -> Report {
        Report {
            class: Class::Release,
            reason: None,
            retry_after_secs: None,
            reversible: true,
            promotion_pending: false,
            detail: None,
        }
    }
}

/// The object as written: every field optional, unknown fields ignored.
#[derive(Deserialize)]
struct Wire {
    class: Option<Class>,
    reason: Option<String>,
    retry_after_secs: Option<u64>,
    reversible: Option<bool>,
    promotion_pending: Option<bool>,
    detail: Option<String>,
}

/// The last line of `stdout` that is not blank.
fn last_line(stdout: &str) -> Option<&str> {
    stdout.lines().rev().find(|line| !line.trim().is_empty())
}

impl Report {
    /// Reads one line as the report object.
    pub fn parse_line(line: &str) -> Result<Report, LineError> {
        let wire: Wire =
            serde_json::from_str(line).map_err(|e| LineError::Malformed(e.to_string()))?;
        wire.reason
            .as_deref()
            .filter(|reason| !is_reason_token(reason))
            .map_or(Ok(()), |reason| {
                Err(LineError::BadReason(reason.to_owned()))
            })?;
        Ok(Report {
            class: wire.class.unwrap_or_default(),
            reason: wire.reason,
            retry_after_secs: wire.retry_after_secs,
            reversible: wire.reversible.unwrap_or(true),
            promotion_pending: wire.promotion_pending.unwrap_or(false),
            detail: wire.detail,
        })
    }

    /// Reads the last non-blank line of `stdout`, strictly.
    pub fn from_stdout(stdout: &str) -> Result<Report, LineError> {
        let line = last_line(stdout).ok_or(LineError::Missing)?;
        Report::parse_line(line)
    }

    /// Reads the last line the way the updater does: a missing or malformed line is `{}`.
    pub fn read(stdout: &str) -> Report {
        Report::from_stdout(stdout).unwrap_or_default()
    }

    /// The line to print: one JSON object, no newline.
    pub fn to_line(&self) -> String {
        let mut object = Map::new();
        object.insert("class".to_owned(), Value::from(self.class.as_str()));
        object.insert("reversible".to_owned(), Value::from(self.reversible));
        object.insert(
            "promotion_pending".to_owned(),
            Value::from(self.promotion_pending),
        );
        let optional = [
            ("reason", self.reason.clone().map(Value::from)),
            ("retry_after_secs", self.retry_after_secs.map(Value::from)),
            ("detail", self.detail.clone().map(Value::from)),
        ];
        for (key, value) in optional {
            object.extend(value.map(|value| (key.to_owned(), value)));
        }
        Value::Object(object).to_string()
    }

    /// Writes the line and a newline: what a hook does last, to its standard output.
    pub fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        writeln!(out, "{}", self.to_line())
    }
}
