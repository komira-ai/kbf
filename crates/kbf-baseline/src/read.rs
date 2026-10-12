//! One buck2 event log read into a [`Build`]: the build's id, result and wall time, and
//! one [`Action`] per action buck2 finished.
//!
//! What each number comes from (message and field names of buck2's `data.proto`):
//!
//! - the build: the invocation header's `trace_id` (the build id buck2 prints); the
//!   `Command` span's `duration` (the build's wall time) and, at its start,
//!   `metadata["buck2_revision"]`; whether it succeeded from the command's result (the
//!   `Result` line, buck2's `CommandResult`): not an `error`, and no `errors` in the
//!   response. (`CommandEnd.is_success` is not used: the buck2 release CI pins writes
//!   `false` there for every build.)
//! - an action: each `ActionExecution` span's end (`ActionExecutionEnd`). Where it ran is
//!   its `execution_kind` ([`Ran::from_execution_kind`]); its time is the span's
//!   `duration` (all buck2 spent on it: cache lookup, upload, execution, download);
//!   `failed` and, from the last of its `commands` (the one buck2 shows), the exit code
//!   and the status (`Success`, `Failure`, `Timeout`, `Error`, `Cancelled`, ...).
//! - the farm's queue and execution times: that command's `metadata`, `queue_duration`
//!   and `execution_time`. buck2 computes both from the `ExecutedActionMetadata` the farm
//!   returns (worker start less queued, execution completed less execution started), so
//!   they are the farm's own report. They are kept only for an action that ran in this
//!   build: an action-cache hit carries the metadata of the run that filled the cache.
//!   The queue time is kept only for remote actions.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// Where an action ran, from `ActionExecutionEnd.execution_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ran {
    /// Executed by the remote farm (`REMOTE`, `REMOTE_WORKER`).
    Remote,
    /// Answered from the remote action cache (`ACTION_CACHE`, `REMOTE_DEP_FILE_CACHE`).
    CacheHit,
    /// Executed on the client (`LOCAL`, `LOCAL_WORKER`).
    Local,
    /// Answered from a cache on the client (`LOCAL_DEP_FILE`, `LOCAL_ACTION_CACHE`).
    LocalCache,
    /// Done by buck2 itself, no command (`SIMPLE`: a file write or symlink; `DEFERRED`).
    Buck2,
    /// `NOT_SET`, or a kind this reader does not know.
    Unknown,
}

impl Ran {
    /// The `ActionExecutionKind` numbers of buck2's `data.proto`.
    #[must_use]
    pub fn from_execution_kind(kind: i64) -> Self {
        match kind {
            2 | 11 => Self::Remote,
            3 | 9 => Self::CacheHit,
            1 | 8 => Self::Local,
            7 | 10 => Self::LocalCache,
            4 | 6 => Self::Buck2,
            _ => Self::Unknown,
        }
    }

    /// The name the report uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::CacheHit => "cache_hit",
            Self::Local => "local",
            Self::LocalCache => "local_cache",
            Self::Buck2 => "buck2",
            Self::Unknown => "unknown",
        }
    }
}

/// One finished action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    /// The target it belongs to (`root//:words`), or what owns it if not a target.
    pub target: String,
    /// `ActionName.category` and `identifier`.
    pub category: String,
    pub identifier: String,
    pub ran: Ran,
    pub failed: bool,
    /// The `ActionExecution` span's duration.
    pub action_us: u64,
    /// The farm's queue time, for an action executed remotely in this build.
    pub queue_us: Option<u64>,
    /// The farm's (or the client's) execution time, for an action executed in this build.
    pub execution_us: Option<u64>,
    /// The last command's exit code, if it has one.
    pub exit_code: Option<i64>,
    /// The last command's status (`Success`, `Failure`, ...), if it ran a command.
    pub status: Option<String>,
}

/// One build: one buck2 command's event log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Build {
    /// The invocation's trace id, which buck2 prints as `Build ID`.
    pub build_id: String,
    /// The buck2 that wrote the log (`2026-09-30-<commit>` for a release).
    pub buck2_revision: Option<String>,
    /// Whether the command succeeded; `None` when the log holds no result (buck2 was
    /// killed, or the log was cut).
    pub success: Option<bool>,
    /// The command's wall time; `None` when the log holds no end of the command.
    pub wall_us: Option<u64>,
    /// Every finished action, in the order buck2 finished them.
    pub actions: Vec<Action>,
}

/// Why a log could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error(
        "{0}: not a JSON event log; this reads `.json-lines` and `.json-lines.gz` (write one \
         with `buck2 build --event-log FILE.json-lines.gz`, or convert a stored log with \
         `buck2 log show PATH > FILE.json-lines`)"
    )]
    Encoding(String),
    #[error("the log is empty: no invocation header")]
    Empty,
    #[error("line {line}: {source}")]
    Json {
        line: usize,
        source: serde_json::Error,
    },
}

/// Reads the log at `path`, gzipped if its name ends in `.gz`.
///
/// # Errors
/// The name is not `*.json-lines` or `*.json-lines.gz`, the file cannot be read, or a
/// line is not the JSON this expects.
pub fn read_path(path: &Path) -> Result<Build, ReadError> {
    let name = path.to_string_lossy();
    let gzip = if name.ends_with(".json-lines.gz") {
        true
    } else if name.ends_with(".json-lines") {
        false
    } else {
        return Err(ReadError::Encoding(name.into_owned()));
    };
    let file = std::fs::File::open(path)?;
    if gzip {
        parse(&mut BufReader::new(flate2::read::GzDecoder::new(file)))
    } else {
        parse(&mut BufReader::new(file))
    }
}

/// Reads a log from `input`: the invocation header, then one stream value per line.
///
/// # Errors
/// `input` fails, holds no header, or a line does not parse.
pub fn parse(input: &mut dyn BufRead) -> Result<Build, ReadError> {
    let mut lines = input.lines().enumerate();
    let header: Header = loop {
        let Some((i, line)) = lines.next() else {
            return Err(ReadError::Empty);
        };
        let line = line?;
        if !line.trim().is_empty() {
            break json(i, &line)?;
        }
    };
    let mut build = Build {
        build_id: header.trace_id.unwrap_or_default(),
        buck2_revision: None,
        success: None,
        wall_us: None,
        actions: Vec::new(),
    };
    for (i, line) in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let value: StreamValue = json(i, &line)?;
        if let Some(result) = value.result {
            build.success = Some(result.succeeded());
        }
        let Some(Event { data: Some(data) }) = value.event else {
            continue;
        };
        if let Some(SpanStart {
            data: Some(SpanStartData {
                command: Some(command),
            }),
        }) = data.span_start
        {
            build.buck2_revision = command.metadata.get("buck2_revision").cloned();
        }
        if let Some(end) = data.span_end {
            let what = end.data.unwrap_or_default();
            if what.command.is_some() {
                build.wall_us = Some(end.duration_us);
            }
            if let Some(action) = what.action_execution {
                build.actions.push(action.into_action(end.duration_us));
            }
        }
    }
    Ok(build)
}

fn json<'a, T: Deserialize<'a>>(i: usize, line: &'a str) -> Result<T, ReadError> {
    match serde_json::from_str(line) {
        Ok(value) => Ok(value),
        Err(source) => Err(ReadError::Json {
            line: i + 1,
            source,
        }),
    }
}

// The subset of buck2's serde encoding read here. Every other field and variant is
// skipped; a `oneof` is an object with one key, so each variant read is an `Option`.

#[derive(Deserialize)]
struct Header {
    trace_id: Option<String>,
}

#[derive(Deserialize)]
struct StreamValue {
    #[serde(rename = "Event")]
    event: Option<Event>,
    #[serde(rename = "Result")]
    result: Option<CommandResult>,
}

/// `CommandResult`: its `result` oneof, keyed `error`, `build_response`, ...
#[derive(Deserialize)]
struct CommandResult {
    #[serde(default)]
    result: BTreeMap<String, serde_json::Value>,
}

impl CommandResult {
    /// Not an `error`, and no `errors` in the response (`BuildResponse.errors`).
    fn succeeded(&self) -> bool {
        for (variant, response) in &self.result {
            let errors = response.get("errors").and_then(serde_json::Value::as_array);
            if variant == "error" || errors.is_some_and(|e| !e.is_empty()) {
                return false;
            }
        }
        true
    }
}

#[derive(Deserialize)]
struct Event {
    data: Option<EventData>,
}

#[derive(Deserialize)]
struct EventData {
    #[serde(rename = "SpanStart")]
    span_start: Option<SpanStart>,
    #[serde(rename = "SpanEnd")]
    span_end: Option<SpanEnd>,
}

#[derive(Deserialize)]
struct SpanStart {
    data: Option<SpanStartData>,
}

#[derive(Deserialize)]
struct SpanStartData {
    #[serde(rename = "Command")]
    command: Option<CommandStart>,
}

#[derive(Deserialize)]
struct CommandStart {
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct SpanEnd {
    #[serde(default)]
    duration_us: u64,
    data: Option<SpanEndData>,
}

#[derive(Default, Deserialize)]
struct SpanEndData {
    /// `CommandEnd`: only its presence is read.
    #[serde(rename = "Command")]
    command: Option<IgnoredAny>,
    #[serde(rename = "ActionExecution")]
    action_execution: Option<ActionExecutionEnd>,
}

#[derive(Deserialize)]
struct ActionExecutionEnd {
    #[serde(default)]
    key: ActionKey,
    #[serde(default)]
    name: ActionName,
    #[serde(default)]
    failed: bool,
    #[serde(default)]
    execution_kind: i64,
    #[serde(default)]
    commands: Vec<CommandExecution>,
}

#[derive(Default, Deserialize)]
struct ActionKey {
    #[serde(default)]
    owner: BTreeMap<String, serde_json::Value>,
}

#[derive(Default, Deserialize)]
struct ActionName {
    #[serde(default)]
    category: String,
    #[serde(default)]
    identifier: String,
}

#[derive(Deserialize)]
struct Label {
    package: String,
    name: String,
}

#[derive(Deserialize)]
struct Owner {
    label: Label,
}

#[derive(Default, Deserialize)]
struct CommandExecution {
    details: Option<CommandDetails>,
    #[serde(default)]
    status: BTreeMap<String, serde_json::Value>,
}

#[derive(Default, Deserialize)]
struct CommandDetails {
    signed_exit_code: Option<i64>,
    metadata: Option<CommandMetadata>,
}

#[derive(Default, Deserialize)]
struct CommandMetadata {
    execution_time_us: Option<u64>,
    queue_duration_us: Option<u64>,
}

impl ActionExecutionEnd {
    fn into_action(mut self, action_us: u64) -> Action {
        let ran = Ran::from_execution_kind(self.execution_kind);
        // The last command is the one buck2 shows; an action with none has no status.
        let last = self.commands.pop().unwrap_or_default();
        let status = last.status.into_keys().next();
        let details = last.details.unwrap_or_default();
        let meta = details.metadata.unwrap_or_default();
        let queue_us = if ran == Ran::Remote {
            meta.queue_duration_us
        } else {
            None
        };
        let execution_us = if matches!(ran, Ran::Remote | Ran::Local) {
            meta.execution_time_us
        } else {
            None
        };
        Action {
            target: owner_name(self.key.owner),
            category: self.name.category,
            identifier: self.name.identifier,
            ran,
            failed: self.failed,
            action_us,
            queue_us,
            execution_us,
            exit_code: details.signed_exit_code,
            status,
        }
    }
}

/// `root//:name` for an owner with a target label (`TargetLabel`, `TestTargetLabel`,
/// `LocalResourceSetup`); the variant's name in parentheses for any other (`BxlKey`,
/// `AnonTarget`), and `()` for none.
fn owner_name(owner: BTreeMap<String, serde_json::Value>) -> String {
    let (variant, value) = owner.into_iter().next().unwrap_or_default();
    match serde_json::from_value::<Owner>(value) {
        Ok(o) => format!("{}:{}", o.label.package, o.label.name),
        Err(_) => format!("({variant})"),
    }
}

impl fmt::Display for Ran {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_execution_kind_maps_to_where_it_ran() {
        // Catches: a kind mapped to the wrong place (the action cache as remote, a local
        // cache as a remote hit), against the numbers of buck2's `ActionExecutionKind`.
        let names: Vec<_> = (0..=12)
            .map(|k| Ran::from_execution_kind(k).to_string())
            .collect();
        assert_eq!(
            names,
            [
                "unknown",     // NOT_SET
                "local",       // LOCAL
                "remote",      // REMOTE
                "cache_hit",   // ACTION_CACHE
                "buck2",       // SIMPLE
                "unknown",     // 5, reserved
                "buck2",       // DEFERRED
                "local_cache", // LOCAL_DEP_FILE
                "local",       // LOCAL_WORKER
                "cache_hit",   // REMOTE_DEP_FILE_CACHE
                "local_cache", // LOCAL_ACTION_CACHE
                "remote",      // REMOTE_WORKER
                "unknown",     // not in this buck2
            ]
        );
    }

    fn owner(json: &str) -> String {
        owner_name(serde_json::from_str(json).expect("an owner"))
    }

    #[test]
    fn an_owner_is_its_target_label_or_its_kind() {
        // Catches: a test target's label lost, or an owner without a label (a BXL key)
        // read as an error rather than named.
        let label = r#"{"label":{"package":"root//p","name":"t"},"configuration":{}}"#;
        assert_eq!(owner(&format!(r#"{{"TargetLabel":{label}}}"#)), "root//p:t");
        assert_eq!(
            owner(&format!(r#"{{"TestTargetLabel":{label}}}"#)),
            "root//p:t"
        );
        assert_eq!(owner(r#"{"BxlKey":{"label":"x"}}"#), "(BxlKey)");
        assert_eq!(owner("{}"), "()");
    }

    #[test]
    fn only_json_logs_are_read() {
        // Catches: a protobuf or zstd log read as JSON lines (a parse error at line 1 that
        // does not say what to do), and a missing file not reported.
        for name in ["x.pb.zst", "x.pb", "x.json-lines.zst", "x.json"] {
            let e = read_path(Path::new(name)).expect_err(name);
            assert!(matches!(e, ReadError::Encoding(_)), "{name}: {e}");
            assert!(e.to_string().contains("buck2 log show"), "{e}");
        }
        let e = read_path(Path::new("does-not-exist.json-lines")).expect_err("absent");
        assert!(matches!(e, ReadError::Io(_)), "{e}");
    }

    #[test]
    fn a_bad_line_is_reported_with_its_number() {
        // Catches: an empty log taken for a build with no actions, and a broken line
        // skipped or reported at the wrong line.
        assert!(matches!(parse(&mut &b"\n\n"[..]), Err(ReadError::Empty)));
        let r = parse(&mut &b"\nnot json\n"[..]);
        assert!(matches!(r, Err(ReadError::Json { line: 2, .. })), "{r:?}");
        let log = "\n{\"trace_id\":\"t\"}\n\n{\"Event\":{\"data\":null}}\n{\"Event\":[]}\n";
        let r = parse(&mut log.as_bytes());
        assert!(matches!(r, Err(ReadError::Json { line: 5, .. })), "{r:?}");
        let b = parse(&mut &log.as_bytes()[..log.len() - 14]).expect("the good lines");
        assert_eq!((b.build_id.as_str(), b.actions.len()), ("t", 0));
    }

    #[test]
    fn an_error_result_is_a_failed_build() {
        // Catches: a command that ended in an error (not a build response) reported as a
        // success; and any other response with no errors reported as a failure.
        let parsed = |result: &str| {
            let log = format!("{{}}\n{{\"Result\":{{\"result\":{result}}}}}\n");
            parse(&mut log.as_bytes()).expect("parse").success
        };
        assert_eq!(parsed(r#"{"error":{"message":"m"}}"#), Some(false));
        assert_eq!(parsed(r#"{"build_response":{"errors":[{}]}}"#), Some(false));
        assert_eq!(parsed(r#"{"build_response":{"errors":[]}}"#), Some(true));
        assert_eq!(parsed(r#"{"test_response":{}}"#), Some(true));
    }

    #[test]
    fn a_failed_remote_action_keeps_its_exit_code_and_farm_times() {
        // Catches: a failed action dropped by the reader, its last command's exit code
        // or status lost, and a cache hit's stale metadata kept as farm time. (The
        // capture tests check the same through the report; this one runs the reader's
        // own copy, which is what the coverage ratchet measures for this file.)
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/m1-failing.json-lines.gz");
        let b = read_path(&path).expect("read the capture");
        assert_eq!(
            b.buck2_revision.as_deref().map(|r| &r[..10]),
            Some("2026-09-30")
        );
        assert_eq!((b.success, b.wall_us), (Some(false), Some(390_620)));
        let rows: Vec<_> = b
            .actions
            .iter()
            .map(|a| {
                (
                    a.target.as_str(),
                    a.ran,
                    a.failed,
                    a.exit_code,
                    a.queue_us,
                    a.execution_us,
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("root//:pause", Ran::CacheHit, false, Some(0), None, None),
                (
                    "root//:verdict",
                    Ran::Remote,
                    true,
                    Some(3),
                    Some(1354),
                    Some(3440)
                ),
            ]
        );
        assert_eq!(b.actions[1].status.as_deref(), Some("Failure"));
    }
}
