//! What a Buck2 event log says of each action.
//!
//! `buck2 build --event-log <path>` writes one JSON object per line: a header (the command
//! line), the events, and a footer (the result). The events that matter here are the ends of
//! action executions, `{"Event":{"data":{"SpanEnd":{"data":{"ActionExecution":{...}}}}}}`:
//! each says whether the action `failed`, carries an `error` when it did, and lists its
//! `commands` with a `status` each (`{"Success":{}}`, or `{"Error":{...}}`, `{"Failure":...}`,
//! `{"Timeout":...}`, `{"Cancelled":...}`). The tests pin that shape with lines taken from
//! a real log of the pinned Buck2.
//!
//! The reader fails closed. A log that is not JSON lines, or that has no action execution in
//! it at all (a build of salted actions always runs some, on the farm or from its cache; a
//! log whose events this reader no longer recognises would otherwise read as no failure),
//! is an error, and the gate counts an error as a failure. A file is read as gzip when it
//! starts with the gzip magic (what `--event-log x.json-lines.gz` writes), else as text.

use std::io::Read as _;
use std::path::Path;

use flate2::read::GzDecoder;
use serde_json::Value;

/// What a log holds of action executions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counts {
    /// Action executions that ended.
    pub actions: u64,
    /// Those that failed: `failed`, an `error`, or a command whose status is not `Success`.
    pub failed: u64,
}

/// The text of the log at `path`, un-gzipped if it is gzip.
fn read(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    let result = if bytes.starts_with(&[0x1f, 0x8b]) {
        GzDecoder::new(bytes.as_slice()).read_to_string(&mut text)
    } else {
        bytes.as_slice().read_to_string(&mut text)
    };
    result.map_err(|e| format!("{}: not a log: {e}", path.display()))?;
    Ok(text)
}

/// Whether the `ActionExecution` of an event says the action did not succeed.
fn failed(action: &Value) -> bool {
    let errored = action.get("error").is_some_and(|e| !e.is_null());
    let command_failed = |c: &Value| c.pointer("/status/Success").is_none();
    let commands = action.get("commands").and_then(Value::as_array);
    action["failed"] != false || errored || commands.is_some_and(|cs| cs.iter().any(command_failed))
}

/// The counts of the event log in `text` (JSON lines).
///
/// # Errors
/// A line that is not a JSON object, or a log with no action execution in it.
pub fn count(text: &str) -> Result<Counts, String> {
    let mut counts = Counts {
        actions: 0,
        failed: 0,
    };
    for (n, line) in text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
    {
        let value: Value =
            serde_json::from_str(line).map_err(|e| format!("line {}: {e}", n + 1))?;
        if !value.is_object() {
            return Err(format!("line {}: not an object", n + 1));
        }
        if let Some(action) = value.pointer("/Event/data/SpanEnd/data/ActionExecution") {
            counts.actions += 1;
            counts.failed += u64::from(failed(action));
        }
    }
    if counts.actions == 0 {
        return Err("no action execution in the event log".to_owned());
    }
    Ok(counts)
}

/// The number of actions that failed in the event log at `path`.
///
/// # Errors
/// The file cannot be read, or [`count`] refuses it.
pub fn failed_actions(path: &Path) -> Result<u64, String> {
    Ok(count(&read(path)?)?.failed)
}

#[cfg(test)]
#[path = "eventlog_tests.rs"]
mod tests;
