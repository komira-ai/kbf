//! The JSON object a step writes as the last line of its standard output.
//!
//! ```json
//! {"class":"release","reason":"...","retry_after_secs":300,"reversible":true,
//!  "promotion_pending":false,"detail":"..."}
//! ```
//!
//! The updater reads the last line leniently ([`Report::read`]), field by field: a missing line,
//! or one that is not a JSON object, is `{}` (a `release` failure class, a step that can still be
//! undone, nothing pending); in an object, each invalid field takes its default and is reported
//! as a warning, except a `reversible` that is present and not a boolean, which reads as `false`.
//! The strict read ([`Report::from_stdout`]) refuses any invalid field, and the conformance
//! harness uses it: it refuses a hook that leaves the line out or writes it badly.

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
    /// The line stated `reversible` (true or false) rather than leaving it out. A step that
    /// leaves it out is an old hook, and `next` never rolls back after a failed `cleanup` on
    /// its account.
    pub reversible_stated: bool,
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
            reversible_stated: false,
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

/// A text field: `None` when absent, or (with a warning) when not text.
fn text_field(
    object: &Map<String, Value>,
    name: &str,
    warnings: &mut Vec<String>,
) -> Option<String> {
    match object.get(name) {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => {
            warnings.push(format!("{name} is not text: ignored"));
            None
        }
    }
}

/// A boolean field: `absent` when missing; `false`, with a warning, when not a boolean.
fn bool_field(
    object: &Map<String, Value>,
    name: &str,
    absent: bool,
    warnings: &mut Vec<String>,
) -> bool {
    match object.get(name) {
        None => absent,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            warnings.push(format!("{name} is not a boolean: read as false"));
            false
        }
    }
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
        // `"reversible": null` is a stated value that is not a boolean: false, as on the lenient
        // path, and not the same as leaving the field out.
        let null = matches!(
            serde_json::from_str::<Value>(line),
            Ok(Value::Object(object)) if object.get("reversible").is_some_and(Value::is_null)
        );
        Ok(Report {
            class: wire.class.unwrap_or_default(),
            reason: wire.reason,
            retry_after_secs: wire.retry_after_secs,
            reversible: !null && wire.reversible.unwrap_or(true),
            reversible_stated: null || wire.reversible.is_some(),
            promotion_pending: wire.promotion_pending.unwrap_or(false),
            detail: wire.detail,
        })
    }

    /// Reads the last non-blank line of `stdout`, strictly.
    pub fn from_stdout(stdout: &str) -> Result<Report, LineError> {
        let line = last_line(stdout).ok_or(LineError::Missing)?;
        Report::parse_line(line)
    }

    /// Reads the last line the way the updater does (see [`Report::read_with_warnings`]).
    pub fn read(stdout: &str) -> Report {
        Report::read_with_warnings(stdout).0
    }

    /// The updater's lenient read: a missing line, or one that is not a JSON object, is `{}`;
    /// otherwise each field is read on its own, and one that is invalid (not a token, the wrong
    /// type, an unknown class) takes its default and is reported in the warnings. A `reversible`
    /// that is present and not a boolean reads as `false`: a step that might have committed is
    /// treated as committed.
    pub fn read_with_warnings(stdout: &str) -> (Report, Vec<String>) {
        let Some(line) = last_line(stdout) else {
            return (Report::default(), Vec::new());
        };
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) else {
            let warning = format!("the last line is not a JSON object: {line}");
            return (Report::default(), vec![warning]);
        };
        let mut warnings = Vec::new();
        let class = match object.get("class") {
            None => Class::Release,
            Some(Value::String(word)) if word == "release" => Class::Release,
            Some(Value::String(word)) if word == "environment" => Class::Environment,
            Some(_) => {
                warnings.push("class is not release or environment: ignored".to_owned());
                Class::Release
            }
        };
        let reason = match text_field(&object, "reason", &mut warnings) {
            Some(reason) if !is_reason_token(&reason) => {
                warnings.push("reason is not a token of [a-z0-9_]: ignored".to_owned());
                None
            }
            other => other,
        };
        let retry_after_secs = match object.get("retry_after_secs") {
            None => None,
            Some(value) => {
                let secs = value.as_u64();
                if secs.is_none() {
                    warnings.push("retry_after_secs is not a whole number: ignored".to_owned());
                }
                secs
            }
        };
        let report = Report {
            class,
            reason,
            retry_after_secs,
            reversible: bool_field(&object, "reversible", true, &mut warnings),
            reversible_stated: object.contains_key("reversible"),
            promotion_pending: bool_field(&object, "promotion_pending", false, &mut warnings),
            detail: text_field(&object, "detail", &mut warnings),
        };
        (report, warnings)
    }

    /// This report with `reversible` stated: the line will carry it, true or false.
    pub fn with_reversible(mut self, reversible: bool) -> Report {
        self.reversible = reversible;
        self.reversible_stated = true;
        self
    }

    /// The line to print: one JSON object, no newline. `reversible` is printed when it was stated
    /// or is false; a report that leaves it out prints without it, as an old hook's does.
    pub fn to_line(&self) -> String {
        let mut object = Map::new();
        object.insert("class".to_owned(), Value::from(self.class.as_str()));
        if self.reversible_stated || !self.reversible {
            object.insert("reversible".to_owned(), Value::from(self.reversible));
        }
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
