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
use std::io;
use std::path::Path;
use std::time::Duration;

/// Denials listed in a note, and in the daemon's log, per lease.
pub const MAX_LINES: usize = 20;

/// The most a note adds to stderr, in bytes.
pub const MAX_NOTE_BYTES: usize = 4096;

/// The longest a target is shown, in characters.
const MAX_TARGET: usize = 300;

/// The least a read looks back, however short the action ran: a `log show` over a few
/// seconds can miss a line a wider one returns (the CI Mac's did, issue #184). The pids
/// are the action's own, so a wider window adds no other lease's lines to its result.
pub const MIN_WINDOW: Duration = Duration::from_secs(120);

/// What a read looks back past the action's start and the wait since its end.
pub const MARGIN: Duration = Duration::from_secs(2);

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
#[cfg(target_os = "macos")]
const READ_BYTES: u64 = 4 << 20;

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

/// How far back a read `waited` after the action ended looks, for an action that ran `ran`.
#[must_use]
pub fn window(ran: Duration, waited: Duration) -> Duration {
    (ran + waited + MARGIN).max(MIN_WINDOW)
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
        let last = self.attempts.last().map_or(Duration::ZERO, |a| a.window);
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
/// shadows it), its output in `file`, read at most [`READ_BYTES`] and given up after
/// [`READ_WITHIN`].
///
/// # Errors
/// `log` could not run or did not end in time; always off macOS.
#[cfg(target_os = "macos")]
pub fn system_log(file: &Path, window: Duration) -> io::Result<String> {
    use std::io::Read as _;
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
    let mut text = String::new();
    let read = std::fs::File::open(file)?
        .take(READ_BYTES)
        .read_to_string(&mut text);
    let _ = std::fs::remove_file(file);
    read?;
    if status.success() {
        Ok(text)
    } else {
        Err(io::Error::other(format!("log show: {status}")))
    }
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
mod tests {
    use super::*;

    const LINE: &str = "2026-10-17 10:00:00.1 Df kernel[0:1] (Sandbox) Sandbox: touch(123) deny(1) file-write-create /work/x/y";

    /// Catches a parser that loses the pid, the operation or the target, takes the
    /// count `(1)` for the pid, or cuts a target with spaces.
    #[test]
    fn a_deny_line_gives_its_pid_operation_and_target() {
        let d = parse_line(LINE).expect("a denial");
        assert_eq!(
            d,
            Denial {
                pid: 123,
                process: "touch".into(),
                operation: "file-write-create".into(),
                target: "/work/x/y".into(),
            }
        );
        let spaced = "x Sandbox: Xcode Helper (Plugin)(77) deny(1) file-write-data /a b/c";
        let d = parse_line(spaced).expect("a denial");
        assert_eq!((d.pid, d.process.as_str()), (77, "Xcode Helper (Plugin)"));
        assert_eq!(
            (d.operation.as_str(), d.target.as_str()),
            ("file-write-data", "/a b/c")
        );
    }

    /// The line of the slot-user run on a CI Mac (kernel is the sender, the subsystem is
    /// `com.apple.sandbox.reporting`, the path is the `/private` form of what the action
    /// wrote), exactly as `log show --style compact` printed it, in a fixture file: the
    /// process in it is a shell, which the interpreter budget counts in test code.
    const KERNEL_LINE: &str = include_str!("../tests/fixtures/kernel_denial.log");

    /// Catches a parser that drops the line the kernel itself reports, with a subsystem
    /// in brackets and no `(Sandbox)` sender, and a pid outside the watched set taken as
    /// the action's.
    #[test]
    fn a_line_the_kernel_reports_under_a_subsystem_is_a_denial() {
        let d = parse_line(KERNEL_LINE).expect("a denial");
        assert_eq!(
            (d.pid, d.operation.as_str(), d.target.as_str()),
            (
                46466,
                "file-write-create",
                "/private/var/tmp/kbf-denied-46082"
            )
        );
        // The process is the name the fixture holds between `Sandbox: ` and `(pid)`.
        assert_eq!(d.process.len(), 4, "{d:?}");
        assert!(KERNEL_LINE.contains(&format!("Sandbox: {}(46466)", d.process)));
        let found = Found::of(KERNEL_LINE, &BTreeSet::from([46466]));
        assert_eq!(found.mine.len(), 1);
        assert!(found.note().expect("note").contains("kbf-denied-46082"));
    }

    /// Catches a predicate that selects only lines whose sender is `Sandbox`: on the
    /// runner the same denial is the kernel's own, under the sandbox reporting
    /// subsystem, and `log show` never returned it to the driver (issue #184).
    #[test]
    fn the_predicate_selects_the_kernel_reported_line_too() {
        assert!(PREDICATE.contains("sender == \"Sandbox\""), "{PREDICATE}");
        assert!(
            PREDICATE.contains("subsystem == \"com.apple.sandbox.reporting\""),
            "{PREDICATE}"
        );
        assert!(
            PREDICATE.contains("eventMessage BEGINSWITH \"Sandbox: \""),
            "{PREDICATE}"
        );
        assert!(
            PREDICATE.contains("eventMessage CONTAINS \"deny(\""),
            "{PREDICATE}"
        );
    }

    /// A log and a clock for [`collect`]: `shown_from` is the read (1-based) from which the
    /// log holds the action's line, `others` makes every read hold another pid's line;
    /// each pause moves the clock, each read costs `cost`.
    struct Fake {
        reads: Vec<Duration>,
        pauses: Vec<Duration>,
        clock: Duration,
        shown_from: usize,
        others: bool,
        cost: Duration,
    }

    impl Fake {
        fn new(shown_from: usize) -> Self {
            Self {
                reads: Vec::new(),
                pauses: Vec::new(),
                clock: Duration::ZERO,
                shown_from,
                others: false,
                cost: Duration::ZERO,
            }
        }

        fn run(&mut self, ran: Duration, schedule: Schedule) -> Report {
            let me = std::cell::RefCell::new(&mut *self);
            collect(
                &BTreeSet::from([123]),
                ran,
                schedule,
                |window| {
                    let mut f = me.borrow_mut();
                    f.reads.push(window);
                    let cost = f.cost;
                    f.clock += cost;
                    let n = f.reads.len();
                    let mut log = String::new();
                    if f.others {
                        log.push_str(&LINE.replace("(123)", "(999)"));
                        log.push('\n');
                    }
                    if n >= f.shown_from {
                        log.push_str(LINE);
                    }
                    Ok(log)
                },
                || me.borrow().clock,
                |d| {
                    let mut f = me.borrow_mut();
                    f.pauses.push(d);
                    f.clock += d;
                },
            )
        }
    }

    const EVERY: Duration = Duration::from_millis(250);

    fn schedule(give_up_ms: u64) -> Schedule {
        Schedule {
            every: EVERY,
            give_up: Duration::from_millis(give_up_ms),
        }
    }

    /// Catches a report that stops at the first empty read (the kernel's line reaches the
    /// log seconds late), one that keeps reading after it has the action's line, and one
    /// that pauses by anything but the schedule's interval.
    #[test]
    fn reads_go_on_until_the_actions_line_appears_then_stop() {
        let mut fake = Fake::new(4);
        let report = fake.run(Duration::from_secs(3), schedule(10_000));
        assert_eq!(fake.reads.len(), 4);
        assert_eq!(fake.pauses, [EVERY; 3]);
        assert_eq!(report.attempts.len(), 4);
        assert_eq!(report.found.mine.len(), 1);
        assert_eq!(
            report.attempts.iter().map(|a| a.mine).collect::<Vec<_>>(),
            [0, 0, 0, 1]
        );
        assert_eq!(report.failed, None);
    }

    /// Catches a report that polls forever, one that gives up before the schedule is
    /// spent, and a read that starts after `give_up`: with 250 ms pauses and 1 s allowed,
    /// reads start at 0, 250, 500, 750 and 1000 ms and no later.
    #[test]
    fn reads_stop_when_the_schedule_is_spent() {
        let mut fake = Fake::new(usize::MAX);
        let report = fake.run(Duration::from_secs(1), schedule(1000));
        assert_eq!(fake.reads.len(), 5, "{:?}", report.attempts);
        let at: Vec<_> = report.attempts.iter().map(|a| a.at.as_millis()).collect();
        assert_eq!(at, [0, 250, 500, 750, 1000]);
        assert!(report.found.mine.is_empty());
        // A read that itself takes time counts: 400 ms each leaves reads at 0 and 650 ms.
        let mut slow = Fake::new(usize::MAX);
        slow.cost = Duration::from_millis(400);
        let report = slow.run(Duration::from_secs(1), schedule(1000));
        let at: Vec<_> = report.attempts.iter().map(|a| a.at.as_millis()).collect();
        assert_eq!(at, [0, 650]);
    }

    /// Catches a log that holds only other processes' denials ending the reads (those
    /// are not the action's own and the action's line may follow).
    #[test]
    fn another_pids_denial_does_not_end_the_reads() {
        let mut fake = Fake::new(3);
        fake.others = true;
        let report = fake.run(Duration::ZERO, schedule(10_000));
        assert_eq!(fake.reads.len(), 3);
        assert_eq!(report.found.mine.len(), 1);
        assert_eq!(report.attempts[0].others, BTreeSet::from([999]));
    }

    /// Catches a failed read that is retried or hidden, and an empty report that does not
    /// say it failed.
    #[test]
    fn a_failed_read_ends_the_report_and_says_why() {
        let mut reads = 0;
        let report = collect(
            &BTreeSet::from([1]),
            Duration::ZERO,
            Schedule::DEFAULT,
            |_| {
                reads += 1;
                Err(io::Error::other("log is down"))
            },
            || Duration::ZERO,
            |_| panic!("no pause after a failed read"),
        );
        assert_eq!(reads, 1);
        assert_eq!(report.failed.as_deref(), Some("log is down"));
        let said = report.explain(&BTreeSet::from([1]));
        assert!(said.contains("the last read failed: log is down"), "{said}");
    }

    /// Catches a window shorter than the floor (the 4 s to 8 s windows of the failing
    /// CI run), one that ignores a long action or the wait since its end, and a reader
    /// handed a different window than the report records.
    #[test]
    fn the_window_has_a_floor_and_covers_the_run_and_the_wait() {
        assert_eq!(window(Duration::from_secs(1), Duration::ZERO), MIN_WINDOW);
        assert_eq!(
            window(Duration::from_secs(300), Duration::from_secs(5)),
            Duration::from_secs(307)
        );
        let mut fake = Fake::new(3);
        let report = fake.run(Duration::from_secs(200), schedule(10_000));
        assert_eq!(fake.reads[0], Duration::from_secs(202));
        assert_eq!(fake.reads[2], Duration::from_millis(202_500));
        let recorded: Vec<_> = report.attempts.iter().map(|a| a.window).collect();
        assert_eq!(recorded, fake.reads);
    }

    /// Catches a query whose recorded text differs from what `log` is run with: both are
    /// built by [`query_args`]; and a window rounded down (a 1.2 s window is 2 s).
    #[test]
    fn the_query_names_its_window_in_whole_seconds_rounded_up() {
        let args = query_args(Duration::from_millis(1200));
        assert_eq!(
            args,
            [
                "show",
                "--style",
                "compact",
                "--last",
                "2s",
                "--predicate",
                PREDICATE
            ]
        );
        assert_eq!(query_args(Duration::ZERO)[4], "1s");
    }

    /// Catches an explanation that omits the query, the pids watched, the sizes of what
    /// came back, the pids rejected or a line with a verdict that did not parse; and one
    /// that lists hundreds of reads.
    #[test]
    fn an_empty_report_explains_the_query_and_why_each_line_was_rejected() {
        let log = format!(
            "{}\nSandbox: odd deny file-write-data /x\nnoise\n",
            LINE.replace("(123)", "(999)")
        );
        let report = collect(
            &BTreeSet::from([5, 6]),
            Duration::from_secs(1),
            schedule(0),
            |_| Ok(log.clone()),
            || Duration::ZERO,
            |_| {},
        );
        let said = report.explain(&BTreeSet::from([5, 6]));
        assert!(said.contains("watched pid(s) {5, 6}"), "{said}");
        assert!(said.contains("1 read(s)"), "{said}");
        assert!(said.contains("--last 120s --predicate"), "{said}");
        assert!(said.contains(PREDICATE), "{said}");
        assert!(said.contains(&format!("{} byte(s)", log.len())), "{said}");
        assert!(said.contains("3 line(s)"), "{said}");
        assert!(said.contains("1 denial(s) read"), "{said}");
        assert!(said.contains("1 line(s) with a verdict not read"), "{said}");
        assert!(said.contains("0 by watched pids"), "{said}");
        assert!(said.contains("pids {999} are not watched"), "{said}");

        let mut many = Fake::new(usize::MAX);
        let report = many.run(Duration::ZERO, schedule(10_000));
        assert_eq!(report.attempts.len(), 41);
        let said = report.explain(&BTreeSet::from([123]));
        assert_eq!(said.matches("; read ").count(), 4, "{said}");
        assert!(said.contains("37 reads between"), "{said}");
    }

    /// Catches an allow line, a line of another sender and an unnumbered process read
    /// as denials.
    #[test]
    fn only_deny_lines_with_a_pid_are_denials() {
        assert_eq!(
            parse_line("Sandbox: touch(1) allow(1) file-write-create /x"),
            None
        );
        assert_eq!(parse_line("no sandbox here"), None);
        assert_eq!(
            parse_line("Sandbox: touch(x) deny(1) file-write-create /x"),
            None
        );
        assert_eq!(
            parse_line("Sandbox: touch() deny(1) file-write-create /x"),
            None
        );
        assert_eq!(
            parse_line("Sandbox: touch(1) denied file-write-create /x"),
            None
        );
        // A pid too large for one, and a line that ends where the verdict does.
        assert_eq!(
            parse_line("Sandbox: touch(99999999999) deny(1) file-write-create /x"),
            None
        );
        let ended = parse_line("Sandbox: touch(1) deny").expect("denial");
        assert_eq!((ended.pid, ended.operation.as_str()), (1, ""));
        // Parentheses that never close, or never name a pid.
        assert_eq!(
            parse_line("Sandbox: touch(12 deny(1) file-write-create /x"),
            None
        );
        assert_eq!(parse_line("Sandbox: touch deny file-write-create /x"), None);
        let bare = parse_line("Sandbox: touch(1) deny file-read-data /x").expect("denial");
        assert_eq!(bare.operation, "file-read-data");
        let none = parse_line("Sandbox: touch(1) deny(1)").expect("denial");
        assert_eq!((none.operation.as_str(), none.target.as_str()), ("", ""));
    }

    /// Catches another lease's denial listed as this one's, and a repeat listed twice.
    #[test]
    fn the_actions_denials_are_apart_from_others_and_without_repeats() {
        let other = LINE.replace("(123)", "(999)");
        let log = format!("{LINE}\n{LINE}\n{other}\nnoise\n");
        let found = Found::of(&log, &BTreeSet::from([123]));
        assert_eq!(found.mine.len(), 1);
        assert_eq!(found.other.len(), 1);
        assert_eq!(found.other[0].pid, 999);
        assert_eq!(
            found.other_lines(),
            ["touch(999) file-write-create /work/x/y"]
        );
        let note = found.note().expect("a note");
        assert!(
            note.contains("kbf:   touch(123) file-write-create /work/x/y\n"),
            "{note}"
        );
        assert!(!note.contains("999"), "{note}");
        assert!(note.contains("1 more denial(s)"), "{note}");
        assert!(note.ends_with('\n'), "{note:?}");
    }

    /// Catches a note for a log with no denials of the action's own: stderr of a
    /// failure that was no sandbox's (a test failing, a compile error) stays the action's.
    #[test]
    fn nothing_of_the_actions_own_is_no_note() {
        assert_eq!(Found::default().note(), None);
        let found = Found::of(LINE, &BTreeSet::new());
        assert_eq!(found.note(), None);
        assert_eq!(found.other_lines().len(), 1);
    }

    /// Catches an unbounded note: many denials, one with a huge target.
    #[test]
    fn a_note_is_capped_in_lines_and_bytes() {
        let mut log = String::new();
        for i in 0..50 {
            log.push_str(&format!("Sandbox: t(1) deny(1) file-write-create /p{i}\n"));
        }
        let found = Found::of(&log, &BTreeSet::from([1]));
        let note = found.note().expect("a note");
        assert_eq!(note.matches("kbf:   t(1)").count(), MAX_LINES);
        assert!(
            note.contains(&format!("... and {} more", 50 - MAX_LINES)),
            "{note}"
        );
        // Long targets of two-byte characters: the cut falls inside one unless moved.
        let mut huge = String::new();
        for i in 0..MAX_LINES {
            huge.push_str(&format!(
                "Sandbox: t(1) deny(1) file-write-create /{i}{}\n",
                "\u{e9}".repeat(5000)
            ));
        }
        let note = Found::of(&huge, &BTreeSet::from([1]))
            .note()
            .expect("a note");
        assert!(note.len() <= MAX_NOTE_BYTES + 4, "{}", note.len());
        assert!(note.ends_with("...\n"), "{note}");
        let shown_target = note.lines().nth(2).expect("a line").chars().count();
        assert!(
            shown_target < MAX_TARGET + 40,
            "a target is cut: {shown_target}"
        );
    }

    /// Catches a log reader off macOS that claims to read something.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn off_macos_there_is_no_sandbox_log() {
        let why =
            system_log(Path::new("/nonexistent"), Duration::from_secs(1)).expect_err("no log");
        assert_eq!(why.kind(), io::ErrorKind::Unsupported);
    }
}
