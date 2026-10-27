//! The log audit of `kbf-guest mkuser`: does the guest's unified log record the generated
//! password that `sysadminctl` (or `dscl`) was given on its command line?
//!
//! Harvesting the guest's logs to the host cannot answer that (the harvest is an allowlist,
//! not an archive), so the question is answered inside the guest, by one root step,
//! `log-audit`, that runs after the account steps and before the ready marker:
//!
//! 1. `log show --last 1h --info --debug --style compact --predicate <PREDICATE>`: the
//!    entries of the account tools, and of anything that mentions a password argument or
//!    `kcpassword`. The step counts the entries ([`Counts::matched_lines`]).
//! 2. `log show --last 1h --info --debug` (everything): the step counts the lines that hold
//!    the password itself ([`Counts::password_hits`]) and the lines that hold any
//!    [`WINDOW`]-character piece of it ([`Counts::window_hits`], so a log that cut or
//!    wrapped the password still shows).
//!
//! # Nothing of the log leaves the step
//! The output of `log show` is read as a stream, line by line, into counters: it is not
//! kept, not logged and not captured as the stdout or stderr of a step frame (the stderr
//! is not read at all). The frame ([`AuditFrame`]) holds the three counts and how the step
//! ended, and no text: not a log line, not the password, not a note. A run that fails to
//! read the log says so by its status and exit code only, with the counts it had.
//!
//! The step is informational. Its status never changes whether the user was made (the
//! summary's `ok`), and it does not gate the ready marker.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::mkuser::MkUserHost;
use crate::sim::{Ended, Status};

/// The id of the step, as its frame names it.
pub const STEP: &str = "log-audit";
/// The name of the frame the step sends.
pub const AUDIT_NAME: &str = "mkuser-audit.json";
/// The macOS log tool.
pub const LOG: &str = "/usr/bin/log";
/// The entries the first pass counts.
pub const PREDICATE: &str = "process == \"sysadminctl\" OR process == \"dscl\" OR eventMessage CONTAINS \"-password\" OR eventMessage CONTAINS[c] \"kcpassword\"";
/// The length of the pieces of the password the second pass looks for.
pub const WINDOW: usize = 8;
/// How long the start of a counted entry is: `YYYY-MM-DD ` (a compact-style entry starts
/// with its date; the header lines and a message's continuation lines do not).
const STAMP: usize = 11;

/// The command of the first pass.
#[must_use]
pub fn matched_argv() -> Vec<String> {
    [
        LOG,
        "show",
        "--last",
        "1h",
        "--info",
        "--debug",
        "--style",
        "compact",
        "--predicate",
        PREDICATE,
    ]
    .map(str::to_owned)
    .to_vec()
}

/// The command of the second pass: the whole log of the last hour.
#[must_use]
pub fn full_argv() -> Vec<String> {
    [LOG, "show", "--last", "1h", "--info", "--debug"]
        .map(str::to_owned)
        .to_vec()
}

/// What the audit counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    /// Entries the first pass printed.
    pub matched_lines: u64,
    /// Lines of the second pass that hold the password.
    pub password_hits: u64,
    /// Lines of the second pass that hold any [`WINDOW`]-character piece of the password
    /// (every line that holds the password is one).
    pub window_hits: u64,
}

/// The frame of the step: counts and how it ended, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditFrame {
    pub step: String,
    pub status: Status,
    /// The exit code of the first `log show` that did not exit 0, or 0.
    pub exit: Option<i32>,
    /// The signal that ended it, if one did.
    pub signal: Option<i32>,
    #[serde(flatten)]
    pub counts: Counts,
}

/// A byte stream cut into lines and counted, without keeping any of it.
struct Scanner {
    password: Vec<u8>,
    /// Every `window`-byte piece of the password.
    pieces: HashSet<Vec<u8>>,
    window: usize,
    /// The first bytes of the current line.
    head: Vec<u8>,
    /// The last bytes of the current line (one fewer than the password is long), so a
    /// piece split between two reads is still found.
    tail: Vec<u8>,
    seen: bool,
    exact: bool,
    piece: bool,
    counts: Counts,
}

impl Scanner {
    /// An empty `password` finds nothing (every line would hold it).
    fn new(password: &str) -> Self {
        let password = password.as_bytes().to_vec();
        let window = WINDOW.min(password.len());
        let pieces = if password.is_empty() {
            HashSet::new()
        } else {
            password.windows(window).map(<[u8]>::to_vec).collect()
        };
        Self {
            password,
            pieces,
            window,
            head: Vec::new(),
            tail: Vec::new(),
            seen: false,
            exact: false,
            piece: false,
            counts: Counts::default(),
        }
    }

    fn feed(&mut self, mut rest: &[u8]) {
        while let Some(i) = rest.iter().position(|&b| b == b'\n') {
            self.part(&rest[..i]);
            self.end_line();
            rest = &rest[i + 1..];
        }
        self.part(rest);
    }

    /// Some bytes of the current line.
    fn part(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.seen = true;
        let room = STAMP.saturating_sub(self.head.len());
        self.head.extend_from_slice(&bytes[..room.min(bytes.len())]);
        if self.pieces.is_empty() {
            return;
        }
        let mut joined = std::mem::take(&mut self.tail);
        joined.extend_from_slice(bytes);
        if joined.windows(self.window).any(|w| self.pieces.contains(w)) {
            self.piece = true;
            self.exact |= joined
                .windows(self.password.len())
                .any(|w| w == self.password.as_slice());
        }
        let keep = (self.password.len() - 1).min(joined.len());
        self.tail = joined.split_off(joined.len() - keep);
    }

    fn end_line(&mut self) {
        if self.is_entry() {
            self.counts.matched_lines += 1;
        }
        self.counts.password_hits += u64::from(self.exact);
        self.counts.window_hits += u64::from(self.piece);
        self.head.clear();
        self.tail.clear();
        self.seen = false;
        self.exact = false;
        self.piece = false;
    }

    /// Whether the line starts like a compact-style entry: `YYYY-MM-DD `.
    fn is_entry(&self) -> bool {
        let h = &self.head;
        h.len() == STAMP
            && h.iter().enumerate().all(|(i, &b)| match i {
                4 | 7 => b == b'-',
                10 => b == b' ',
                _ => b.is_ascii_digit(),
            })
    }

    /// The counts, once the stream ended (a last line without a newline is a line).
    fn finish(mut self) -> Counts {
        if self.seen {
            self.end_line();
        }
        self.counts
    }
}

/// Runs `argv` and counts its stdout into `scanner`; how it ended.
fn pass(
    host: &dyn MkUserHost,
    argv: &[String],
    deadline: std::time::Duration,
    scanner: &mut Scanner,
) -> Ended {
    host.stream(argv, deadline, &mut |chunk| scanner.feed(chunk))
        .ended
}

/// Runs the audit for `password`, each pass with `deadline`.
#[must_use]
pub fn audit(host: &dyn MkUserHost, password: &str, deadline: std::time::Duration) -> AuditFrame {
    let mut first = Scanner::new("");
    let mut second = Scanner::new(password);
    let ended_first = pass(host, &matched_argv(), deadline, &mut first);
    let ended_second = pass(host, &full_argv(), deadline, &mut second);
    let (matched, hits) = (first.finish(), second.finish());
    let counts = Counts {
        matched_lines: matched.matched_lines,
        password_hits: hits.password_hits,
        window_hits: hits.window_hits,
    };
    let (status, exit, signal) = [ended_first, ended_second]
        .into_iter()
        .find(|e| *e != Ended::Exited(0))
        .map_or((Status::Ok, Some(0), None), |e| match e {
            Ended::Exited(code) => (Status::Failed, Some(code), None),
            Ended::Signaled(sig) => (Status::Failed, None, Some(sig)),
            Ended::TimedOut => (Status::Timeout, None, None),
            Ended::SpawnFailed(_) => (Status::Error, None, None),
        });
    AuditFrame {
        step: STEP.to_owned(),
        status,
        exit,
        signal,
        counts,
    }
}
