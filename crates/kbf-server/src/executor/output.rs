//! What a job that did not end well wrote, attached to its action's `Result`.
//!
//! A job that ends without success (any verdict but an ingest) may have died before it wrote
//! its pack, and its output is all that says why. The executor reads its tails once: those of
//! the `GetResult` it has already read, or, when they hold nothing, one `GetLogs`
//! ([`Api::logs_tail`](super::api::Api::logs_tail)). Nothing is polled or streamed, and a
//! job that ended well is never asked.
//!
//! The text is the job's. Each stream is read as UTF-8 (a byte that is not is replaced), and the
//! two together are cut to their last [`OUTPUT_CAP`] bytes, on a character boundary.
//!
//! The result is a `google.rpc.ErrorInfo` of reason [`OUTPUT_REASON`] in the details of the
//! `Result`'s status, its metadata holding `stdout`, `stderr` and `truncated` (`true` when the
//! service or this cap dropped any of the output), and `note` when the output could not be
//! read. The status's code and message are what they were: the farm reads only those, logs
//! the message, and ignores a detail it does not look for. The log line of the output holds
//! lengths only; the end of the standard error, at most
//! [`Config::failed_stderr_log_bytes`](super::Config::failed_stderr_log_bytes) bytes of it, is
//! a second line, at WARN ([`Output::stderr_excerpt`]). Standard output is never logged.

use std::collections::BTreeMap;
use std::fmt;

use js_proto::v1::{GetLogsResponse, JobResult};
use kbf_front::ERROR_DOMAIN;
use kbf_job_client::Error;
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::worker;
use prost::Message;

/// The most bytes of a job's output attached to its `Result`: standard output and standard
/// error together, the end of each kept.
pub(super) const OUTPUT_CAP: usize = 8 << 10;

/// The `ErrorInfo.reason` of a failed job's output.
pub(super) const OUTPUT_REASON: &str = "JOB_OUTPUT_TAIL";

const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// The tails of a job's two streams as the job service holds them: it
/// formats as their lengths, never as the text.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Tail {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    /// The service cut a stream: the job wrote more than its tail holds.
    pub(super) truncated: bool,
}

impl fmt::Debug for Tail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tail")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl Tail {
    /// The tails of `result`, if they hold anything.
    pub(super) fn of_result(result: &JobResult) -> Option<Self> {
        let tail = Self {
            stdout: result.stdout_tail.clone(),
            stderr: result.stderr_tail.clone(),
            truncated: result.truncated,
        };
        (!tail.stdout.is_empty() || !tail.stderr.is_empty()).then_some(tail)
    }

    /// The tails of a `GetLogs` answer.
    pub(super) fn of_logs(logs: GetLogsResponse) -> Self {
        Self {
            stdout: logs.stdout_tail,
            stderr: logs.stderr_tail,
            truncated: logs.truncated,
        }
    }
}

/// What is attached of a failed job's output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Output {
    /// The end of its standard output.
    pub(super) stdout: String,
    /// The end of its standard error.
    pub(super) stderr: String,
    /// The service or [`OUTPUT_CAP`] dropped some of the output.
    pub(super) truncated: bool,
    /// Why the output could not be read, when it could not.
    pub(super) note: Option<String>,
}

impl Output {
    /// The output of `tail`, each stream read as UTF-8 and the two cut to [`OUTPUT_CAP`].
    pub(super) fn of(tail: &Tail) -> Self {
        let read = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        let (stdout, stderr) = (read(&tail.stdout), read(&tail.stderr));
        let (out_cap, err_cap) = shares(stdout.len(), stderr.len(), OUTPUT_CAP);
        let (stdout, cut_out) = ending(&stdout, out_cap);
        let (stderr, cut_err) = ending(&stderr, err_cap);
        Self {
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            truncated: tail.truncated || cut_out || cut_err,
            note: None,
        }
    }

    /// No output, because the call that reads it failed with `error`.
    pub(super) fn unread(error: &Error) -> Self {
        Self {
            note: Some(format!(
                "the job's output could not be read: {}",
                short(error)
            )),
            ..Self::default()
        }
    }

    /// The end of the standard error for the log: its last `cap` bytes or fewer, starting on
    /// a character boundary, without the white space at its ends; `None` when `cap` is 0 or
    /// the stream holds only white space.
    pub(super) fn stderr_excerpt(&self, cap: usize) -> Option<&str> {
        let (text, _) = ending(&self.stderr, cap);
        let text = text.trim();
        (!text.is_empty()).then_some(text)
    }

    /// Whether there is nothing to attach: no text and no note.
    pub(super) fn is_empty(&self) -> bool {
        self.stdout.is_empty() && self.stderr.is_empty() && self.note.is_none()
    }

    /// Adds this output to the details of `result`'s status, which is not OK; the code and
    /// the message stay as they are. Nothing is added when there is nothing to say.
    pub(super) fn attach(&self, result: &mut worker::Result) {
        let Some(status) = result.status.as_mut().filter(|_| !self.is_empty()) else {
            return;
        };
        let mut metadata = BTreeMap::from([
            ("stdout".to_owned(), self.stdout.clone()),
            ("stderr".to_owned(), self.stderr.clone()),
            ("truncated".to_owned(), self.truncated.to_string()),
        ]);
        if let Some(note) = &self.note {
            metadata.insert("note".to_owned(), note.clone());
        }
        let info = ErrorInfo {
            reason: OUTPUT_REASON.to_owned(),
            domain: ERROR_DOMAIN.to_owned(),
            metadata: metadata.into_iter().collect(),
        };
        status.details.push(prost_types::Any {
            type_url: ERROR_INFO.to_owned(),
            value: info.encode_to_vec(),
        });
    }
}

/// How the bytes of `cap` are shared between an output of `out` bytes and an error of `err`:
/// half each, and what one of them does not need goes to the other.
pub(super) fn shares(out: usize, err: usize, cap: usize) -> (usize, usize) {
    let half = cap / 2;
    match (out <= half, err <= half) {
        (true, _) => (out, err.min(cap - out)),
        (false, true) => (out.min(cap - err), err),
        (false, false) => (half, cap - half),
    }
}

/// The last `cap` bytes of `text` or fewer, starting on a character boundary, and whether
/// any of it was dropped.
pub(super) fn ending(text: &str, cap: usize) -> (&str, bool) {
    let from = (text.len().saturating_sub(cap)..=text.len())
        .find(|&at| text.is_char_boundary(at))
        .unwrap_or(text.len());
    (&text[from..], from > 0)
}

/// What went wrong with a read of the output, in a few words: the kind of failure and the
/// status code, never the service's message.
fn short(error: &Error) -> String {
    match error {
        Error::Unreachable(_) => "the call was not sent".to_owned(),
        Error::Transport(_) => "the call was not answered".to_owned(),
        Error::Decode(_) => "the answer could not be decoded".to_owned(),
        Error::Status { code, .. } => format!("the job service answered {code:?}"),
        Error::Refused(_) | Error::TooMany(_) | Error::Stuck(_) => {
            "the job service gave an answer a read does not have".to_owned()
        }
    }
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
