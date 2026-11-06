//! What a job that did not end well wrote, attached to its action's `Result`.
//!
//! A job that ends without success (any verdict but an ingest) may have died before it wrote
//! its pack, and its output is all that says why. The executor reads its tails once: those of
//! the `GetResult` it has already read, or, when they hold nothing, one `GetLogs`
//! ([`Api::logs_tail`](super::api::Api::logs_tail)). Nothing is polled or streamed, and a
//! job that ended well is never asked.
//!
//! The text is the job's, and the job was handed secrets: the presigned url of its pack, the
//! read token, and whatever else the submit put in its files or environment ([`Grants`]).
//! Every occurrence of each is replaced by [`REDACTED`] before anything else is done with the
//! text, so the cap below never cuts a secret in two and leaves its first half. A stream the
//! job service had already cut may begin with the end of a secret: a head that is the last
//! [`MIN_CUT_SECRET`] bytes or more of one is redacted too. Then each stream is read as UTF-8
//! (a byte that is not is replaced), and the two together are cut to their last
//! [`OUTPUT_CAP`] bytes, on a character boundary.
//!
//! The result is a `google.rpc.ErrorInfo` of reason [`OUTPUT_REASON`] in the details of the
//! `Result`'s status, its metadata holding `stdout`, `stderr` and `truncated` (`true` when the
//! service or this cap dropped any of the output), and `note` when the output could not be
//! read. The status's code and message are what they were: the farm reads only those, logs
//! the message, and ignores a detail it does not look for. The text is never logged here:
//! only its lengths are.

use std::collections::BTreeMap;
use std::fmt;

use js_proto::v1::{GetLogsResponse, JobResult, JobSpec};
use kbf_front::ERROR_DOMAIN;
use kbf_job_client::Error;
use kbf_proto::google::rpc::ErrorInfo;
use kbf_proto::worker;
use prost::Message;

use crate::executor::task::Secret;

/// The most bytes of a job's output attached to its `Result`: standard output and standard
/// error together, the end of each kept.
pub(super) const OUTPUT_CAP: usize = 8 << 10;

/// What a secret of the job is replaced by.
pub(super) const REDACTED: &str = "[redacted]";

/// The fewest bytes of the end of a secret that are redacted at the head of a stream the job
/// service cut. Shorter ones are left: they say too little to be worth hiding, and would
/// redact ordinary text that happens to end the same way.
pub(super) const MIN_CUT_SECRET: usize = 8;

/// The `ErrorInfo.reason` of a failed job's output.
pub(super) const OUTPUT_REASON: &str = "JOB_OUTPUT_TAIL";

const ERROR_INFO: &str = "type.googleapis.com/google.rpc.ErrorInfo";

/// The values the executor handed one job, which never appear in what is attached of its
/// output. It formats as a count, never as the values.
#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Grants(Vec<Secret>);

impl fmt::Debug for Grants {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Grants({} redacted)", self.0.len())
    }
}

impl Grants {
    /// The grants of the job `spec`: `secrets`, each file's contents and each environment
    /// value of the spec, and the query of each url among them (a presigned url's signature,
    /// which is the credential on its own). Empty values are left out.
    pub(super) fn of<'a>(spec: &'a JobSpec, secrets: impl IntoIterator<Item = &'a str>) -> Self {
        let named = secrets.into_iter();
        let files = spec.files.iter().map(|file| file.contents.as_str());
        let env = spec.env.values().map(String::as_str);
        let mut values: Vec<&str> = named.chain(files).chain(env).collect();
        let queries: Vec<&str> = values
            .iter()
            .filter_map(|value| value.split_once('?').map(|(_, query)| query))
            .collect();
        values.extend(queries);
        let mut grants: Vec<Secret> = Vec::new();
        for value in values.into_iter().filter(|value| !value.is_empty()) {
            let secret = Secret::new(value);
            if !grants.contains(&secret) {
                grants.push(secret);
            }
        }
        Self(grants)
    }

    /// `text` with every byte inside an occurrence of a grant replaced, each run of such
    /// bytes by one [`REDACTED`]. With `cut_head`, the text is the tail of a longer stream,
    /// and a head that is the last [`MIN_CUT_SECRET`] or more bytes of a grant is a run too.
    pub(super) fn redact(&self, text: &[u8], cut_head: bool) -> Vec<u8> {
        let mut hidden = vec![false; text.len()];
        for grant in &self.0 {
            let grant = grant.expose().as_bytes();
            for at in 0..text.len() {
                if text[at..].starts_with(grant) {
                    hidden[at..at + grant.len()].fill(true);
                }
            }
            if cut_head {
                let head = (MIN_CUT_SECRET..grant.len())
                    .rev()
                    .find(|&n| text.starts_with(&grant[grant.len() - n..]));
                if let Some(n) = head {
                    hidden[..n].fill(true);
                }
            }
        }
        let mut out = Vec::with_capacity(text.len());
        for (at, byte) in text.iter().enumerate() {
            match (hidden[at], at > 0 && hidden[at - 1]) {
                (false, _) => out.push(*byte),
                (true, false) => out.extend_from_slice(REDACTED.as_bytes()),
                (true, true) => {}
            }
        }
        out
    }
}

/// The tails of a job's two streams as the job service holds them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Tail {
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    /// The service cut a stream: the job wrote more than its tail holds.
    pub(super) truncated: bool,
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
    /// The end of its standard output, redacted.
    pub(super) stdout: String,
    /// The end of its standard error, redacted.
    pub(super) stderr: String,
    /// The service or [`OUTPUT_CAP`] dropped some of the output.
    pub(super) truncated: bool,
    /// Why the output could not be read, when it could not.
    pub(super) note: Option<String>,
}

impl Output {
    /// The output of `tail`, redacted of `grants` and cut to [`OUTPUT_CAP`].
    pub(super) fn of(tail: &Tail, grants: &Grants) -> Self {
        let read = |bytes: &[u8]| {
            let redacted = grants.redact(bytes, tail.truncated);
            String::from_utf8_lossy(&redacted).into_owned()
        };
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
