//! What the macOS sandbox refused an action, for the operator.
//!
//! An action under the sandbox ([`crate::network`]) that writes outside its lease
//! directory sees only `EPERM`, which reads like any file-permission error. The
//! refusal is in the unified log, one `Sandbox: <process>(<pid>) deny(1) <operation>
//! <target>` line each. When an action fails, the driver reads the log for the
//! last minutes ([`system_log`]), keeps the lines of the processes it watched
//! ([`Found`]), and adds them to the end of the action's stderr ([`Found::note`]) and
//! to the daemon's log, both capped ([`MAX_LINES`], [`MAX_NOTE_BYTES`]).
//!
//! The unified log shows a kernel-reported line seconds after the refusal, so [`collect`]
//! reads again every [`Schedule::every`] until a line of the action's own appears or
//! [`Schedule::give_up`] has passed; when none appears, [`Report::explain`] says what
//! was asked and what came back, for the node's log.
//!
//! A read looks back only over the action's own run, the wait since it ended and
//! [`MARGIN`] ([`window`]): what `log show` reads, and so how long it takes, is the
//! whole system's log over that window, every other process's lines included, and a
//! busy host logs a lot. The other processes' lines are read but never listed in this
//! lease's result ([`Found`]).
//!
//! Which processes are the action's is known from the polls of its process tree (or
//! its slot user's uid), 250 ms apart by default: a process that starts and ends
//! between two polls is not seen. Its lines, and those of other leases' processes,
//! are counted as "other" and listed in the daemon's log only, never in this lease's
//! result: a result must not carry another lease's paths.
//!
//! A refusal cannot be attributed by uid instead: the log line names the process and
//! its pid and nothing else (the entry holds no uid, and its process is the kernel), and
//! the process may be gone when the log is read. [`Report::explain`] says, in the node's
//! log, which pids a log named when none was watched.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// Denials listed in a note, and in the daemon's log, per lease.
pub const MAX_LINES: usize = 20;

/// The most a note adds to stderr, in bytes.
pub const MAX_NOTE_BYTES: usize = 4096;

/// The longest a target is shown, in characters.
const MAX_TARGET: usize = 300;

/// What a read looks back past the action's start and the wait since its end: a line is
/// stamped when the refusal happened, which is inside the run, so the margin is for the
/// rounding of `--last` to whole seconds and the time between the driver's clock read and
/// `log show`'s own.
pub const MARGIN: Duration = Duration::from_secs(10);

/// When [`collect`] reads, and when it gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    /// The pause between one read and the next.
    pub every: Duration,
    /// No read starts later than this after the action ended.
    pub give_up: Duration,
}

impl Schedule {
    /// A read every 250 ms for 10 s, which covers the 6 s or more the CI Mac's kernel line
    /// took to reach the log.
    pub const DEFAULT: Self = Self {
        every: Duration::from_millis(250),
        give_up: Duration::from_secs(10),
    };
}

/// How long one read of the log may take before it is given up, and what of its
/// output is looked at.
#[cfg(target_os = "macos")]
const READ_WITHIN: Duration = Duration::from_secs(30);

/// The most of a read's output that is looked at, counted from its end.
pub const READ_BYTES: u64 = 4 << 20;

/// The `log show` predicate that selects the sandbox's lines. Where the line is
/// logged from differs by macOS release and by profile: sender `Sandbox`, or the
/// kernel itself under the `com.apple.sandbox.reporting` subsystem; every form's
/// message begins `Sandbox: `. The last clause takes any message holding a verdict, so a
/// form none of the others names still reaches [`parse_line`], which decides.
pub const PREDICATE: &str = "sender == \"Sandbox\" OR subsystem == \"com.apple.sandbox.reporting\" OR eventMessage BEGINSWITH \"Sandbox: \" OR eventMessage CONTAINS \"deny(\"";

/// The arguments of `/usr/bin/log` for a read of the last `window`, whole seconds rounded up.
#[must_use]
pub fn query_args(window: Duration) -> Vec<String> {
    let secs = window.as_secs() + u64::from(window.subsec_nanos() > 0);
    ["show", "--style", "compact", "--last"]
        .into_iter()
        .map(str::to_owned)
        .chain([format!("{}s", secs.max(1))])
        .chain(["--predicate".to_owned(), PREDICATE.to_owned()])
        .collect()
}

/// How far back a read `waited` after the action ended looks, for an action that ran `ran`:
/// the run, the wait and [`MARGIN`], and no more, since the cost of a read is the whole
/// host's log over its window.
#[must_use]
pub fn window(ran: Duration, waited: Duration) -> Duration {
    ran + waited + MARGIN
}

/// A reader of the sandbox's lines of the last `window`: it may write to the scratch file
/// `file` (a file, not a pipe: a child another thread spawns can inherit a pipe and hold
/// it open, issue #481). A function, so a test replaces the system's log.
pub type LogReader = fn(file: &Path, window: Duration) -> io::Result<String>;

/// One refusal: `<process>(<pid>) deny(1) <operation> <target>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Denial {
    pub pid: i32,
    pub process: String,
    pub operation: String,
    pub target: String,
}

/// The refusal in one line of `log show --style compact`: what follows `Sandbox: `, up
/// to the first `(<digits>) deny` (a process name may hold parentheses and spaces),
/// then the operation, then the rest of the line as the target. `allow` lines and
/// other text are `None`.
#[must_use]
pub fn parse_line(line: &str) -> Option<Denial> {
    let s = &line[line.find("Sandbox: ")? + "Sandbox: ".len()..];
    let mut from = 0;
    loop {
        let open = from + s[from..].find('(')?;
        let close = open + s[open..].find(')')?;
        let digits = &s[open + 1..close];
        let denied = s[close + 1..]
            .strip_prefix(" deny")
            .filter(|r| r.is_empty() || r.starts_with(['(', ' ']));
        if let Some(rest) = denied
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(pid) = digits.parse::<i32>()
        {
            // `deny(1) file-write-create /path`: past the parenthesised count.
            let after = rest.split_once(' ').map_or("", |(_, after)| after);
            let (operation, target) = after.split_once(' ').unwrap_or((after, ""));
            return Some(Denial {
                pid,
                process: s[..open].to_owned(),
                operation: operation.to_owned(),
                target: target.trim().to_owned(),
            });
        }
        from = open + 1;
    }
}

/// The refusals in a log, split by whether the process was the action's.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// The action's own, without repeats, sorted.
    pub mine: Vec<Denial>,
    /// Those of other processes, without repeats, sorted.
    pub other: Vec<Denial>,
}

impl Found {
    /// The refusals in `log`, those of `pids` apart from the rest.
    #[must_use]
    pub fn of(log: &str, pids: &BTreeSet<i32>) -> Self {
        let (mut mine, mut other) = (BTreeSet::new(), BTreeSet::new());
        for denial in log.lines().filter_map(parse_line) {
            if pids.contains(&denial.pid) {
                mine.insert(denial);
            } else {
                other.insert(denial);
            }
        }
        Self {
            mine: mine.into_iter().collect(),
            other: other.into_iter().collect(),
        }
    }

    /// The text added to a failed action's stderr: each refusal of its own, capped, and
    /// how many were other processes'; `None` when none of its own was seen (the
    /// others', which on a busy node are other leases' mostly, stay in the node's log).
    #[must_use]
    pub fn note(&self) -> Option<String> {
        if self.mine.is_empty() {
            return None;
        }
        let mut note = String::new();
        let _ = writeln!(
            note,
            "\nkbf: the macOS sandbox denied this action {} operation(s); it may write only \
             inside its lease directory and /dev (plus the few names its tools use in \
             the user's temporary folder):",
            self.mine.len()
        );
        for d in self.mine.iter().take(MAX_LINES) {
            let _ = writeln!(note, "kbf:   {}", shown(d));
        }
        if self.mine.len() > MAX_LINES {
            let _ = writeln!(note, "kbf:   ... and {} more", self.mine.len() - MAX_LINES);
        }
        if !self.other.is_empty() {
            let _ = writeln!(
                note,
                "kbf: {} more denial(s) by processes this lease did not watch (another \
                 lease's, or one that ended between polls) are in the node's log",
                self.other.len()
            );
        }
        if note.len() > MAX_NOTE_BYTES {
            note.truncate(note.floor_char_boundary(MAX_NOTE_BYTES));
            note.push_str("...\n");
        }
        Some(note)
    }

    /// The refusals of other processes, for the node's log, capped.
    #[must_use]
    pub fn other_lines(&self) -> Vec<String> {
        self.other.iter().take(MAX_LINES).map(shown).collect()
    }
}

/// One read of the log, for [`Report::explain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// How far back it looked.
    pub window: Duration,
    /// How long after the action ended it started.
    pub at: Duration,
    /// The size of what came back.
    pub bytes: usize,
    /// Non-empty lines of it.
    pub lines: usize,
    /// Denials among them that [`parse_line`] read.
    pub parsed: usize,
    /// Lines that name a sandbox verdict but that [`parse_line`] could not read.
    pub unparsed: usize,
    /// How many of the parsed denials were by a pid the action watched.
    pub mine: usize,
    /// The pids of the others, none of them watched.
    pub others: BTreeSet<i32>,
}

/// What [`collect`] did and found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// From the last read.
    pub found: Found,
    /// Every read, in order.
    pub attempts: Vec<Attempt>,
    /// Why the reads ended early: a read failed.
    pub failed: Option<String>,
    /// The window of the last read asked for, the failed one included.
    pub asked: Duration,
}

/// Reads the log until it holds a denial by one of `pids`, or no read would start before
/// `schedule.give_up` after the action ended (`ran` is how long it ran). `read(window)`
/// returns the log's text, `now()` the time since the action ended, `pause(d)` waits `d`:
/// functions, so a test has a log and a clock of its own. A failed read ends the report.
pub fn collect(
    pids: &BTreeSet<i32>,
    ran: Duration,
    schedule: Schedule,
    mut read: impl FnMut(Duration) -> io::Result<String>,
    mut now: impl FnMut() -> Duration,
    mut pause: impl FnMut(Duration),
) -> Report {
    let mut report = Report::default();
    loop {
        let at = now();
        let window = window(ran, at);
        report.asked = window;
        let log = match read(window) {
            Ok(log) => log,
            Err(why) => {
                report.failed = Some(why.to_string());
                return report;
            }
        };
        let found = Found::of(&log, pids);
        report.attempts.push(Attempt {
            window,
            at,
            bytes: log.len(),
            lines: log.lines().filter(|l| !l.trim().is_empty()).count(),
            parsed: log.lines().filter_map(parse_line).count(),
            unparsed: log
                .lines()
                .filter(|l| l.contains("Sandbox: ") && l.contains("deny"))
                .filter(|l| parse_line(l).is_none())
                .count(),
            mine: found.mine.len(),
            others: found.other.iter().map(|d| d.pid).collect(),
        });
        let done = !found.mine.is_empty();
        report.found = found;
        if done || now() + schedule.every > schedule.give_up {
            return report;
        }
        pause(schedule.every);
    }
}

impl Report {
    /// For the node's log, when no read held a denial of the action's own: the query,
    /// the pids watched, and for each read what came back and why none counted. The
    /// first and last two reads only when there are many.
    #[must_use]
    pub fn explain(&self, watched: &BTreeSet<i32>) -> String {
        let mut out = String::new();
        let last = self.asked;
        let _ = write!(
            out,
            "no denial by the {} watched pid(s) {watched:?} in {} read(s) of `/usr/bin/log {}`",
            watched.len(),
            self.attempts.len(),
            query_args(last).join(" ")
        );
        if let Some(why) = &self.failed {
            let _ = write!(out, "; the last read failed: {why}");
        }
        let n = self.attempts.len();
        for (i, a) in self.attempts.iter().enumerate() {
            if n > 4 && (2..n - 2).contains(&i) {
                if i == 2 {
                    let _ = write!(out, "; ... {} reads between ...", n - 4);
                }
                continue;
            }
            let _ = write!(
                out,
                "; read {} at +{:.2?} over {:?}: {} byte(s), {} line(s), {} denial(s) read, {} line(s) with a verdict not read, {} by watched pids",
                i + 1,
                a.at,
                a.window,
                a.bytes,
                a.lines,
                a.parsed,
                a.unparsed,
                a.mine,
            );
            if !a.others.is_empty() {
                let _ = write!(out, ", rejected: pids {:?} are not watched", a.others);
            }
        }
        out
    }
}

/// `<process>(<pid>) <operation> <target>`, a long target cut.
pub(crate) fn shown(d: &Denial) -> String {
    let target: String = d.target.chars().take(MAX_TARGET).collect();
    let cut = if target.len() < d.target.len() {
        "..."
    } else {
        ""
    };
    format!("{}({}) {} {target}{cut}", d.process, d.pid, d.operation)
}

/// The sandbox's lines of the last `window` from the system's unified log, through
/// `/usr/bin/log show` ([`query_args`]; the full path: over ssh, zsh's builtin `log`
/// shadows it), its output in `file`, the last [`READ_BYTES`] of it read ([`read_tail`]) and given up after
/// [`READ_WITHIN`].
///
/// # Errors
/// `log` could not run or did not end in time; always off macOS.
#[cfg(target_os = "macos")]
pub fn system_log(file: &Path, window: Duration) -> io::Result<String> {
    use std::process::{Command, Stdio};
    use std::time::Instant;

    let out = std::fs::File::create(file)?;
    let mut child = Command::new("/usr/bin/log")
        .args(query_args(window))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(Stdio::null())
        .spawn()?;
    let give_up = Instant::now() + READ_WITHIN;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= give_up {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(file);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("log show took more than {READ_WITHIN:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let tail = std::fs::File::open(file).and_then(|f| read_tail(f, READ_BYTES));
    let _ = std::fs::remove_file(file);
    let text = tail?;
    if status.success() {
        Ok(text)
    } else {
        Err(io::Error::other(format!("log show: {status}")))
    }
}

/// The last `cap` bytes of `source` as text. `log show` prints oldest first, so when the
/// output is longer than `cap` the newest lines, the denial among them, are the ones to
/// keep. The cut falls anywhere: the partial first line is dropped, and bytes that are not
/// UTF-8 (a character split by the cut, or anything else) become U+FFFD instead of
/// failing the read. Output of `cap` bytes or fewer is returned whole.
///
/// # Errors
/// `source` could not be read.
pub fn read_tail(mut source: impl Read + Seek, cap: u64) -> io::Result<String> {
    let len = source.seek(SeekFrom::End(0))?;
    // One byte before the last `cap`, so a cut at a line start is known to be one.
    let from = len.saturating_sub(cap.saturating_add(1));
    source.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    source.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    let mut kept = bytes.as_slice();
    if len > cap {
        // Whatever precedes the first newline is a line the cut may have split.
        kept = kept
            .iter()
            .position(|&b| b == b'\n')
            .map_or(&[][..], |i| &kept[i + 1..]);
    }
    Ok(String::from_utf8_lossy(kept).into_owned())
}

/// The sandbox's lines of the last `window`: macOS only.
///
/// # Errors
/// Always: there is no sandbox log off macOS.
#[cfg(not(target_os = "macos"))]
pub fn system_log(_: &Path, _: Duration) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no sandbox log off macOS",
    ))
}

#[cfg(test)]
#[path = "denials_tests.rs"]
mod tests;
