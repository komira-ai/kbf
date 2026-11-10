//! Unit tests of [`super`]: the parser, the note, the reads' schedule and windows.

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
    /// Text every read holds after the lines above.
    noise: &'static str,
    /// Every read fails with this.
    broken: Option<&'static str>,
}

/// The most reads a [`Fake`] serves: a report that never stops fails on this, by
/// assertion, instead of looping until the test runner's timeout.
const MOST_READS: usize = 100;

impl Fake {
    fn new(shown_from: usize) -> Self {
        Self {
            reads: Vec::new(),
            pauses: Vec::new(),
            clock: Duration::ZERO,
            shown_from,
            others: false,
            cost: Duration::ZERO,
            noise: "",
            broken: None,
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
                assert!(
                    f.reads.len() <= MOST_READS,
                    "more than {MOST_READS} reads: the report does not stop"
                );
                if let Some(why) = f.broken {
                    return Err(io::Error::other(why));
                }
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
                log.push_str(f.noise);
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

/// Catches a report with no bound that holds with the default schedule and a source
/// that never yields the action's line: it must give up within `give_up` (the last read
/// starts at or before it, and the clock never passes it by more than one pause), by
/// assertion on the fake clock and not by hanging.
#[test]
fn a_source_that_never_yields_is_given_up_within_the_schedule() {
    let mut fake = Fake::new(usize::MAX);
    let report = fake.run(Duration::from_secs(5), Schedule::DEFAULT);
    let last = report.attempts.last().expect("a read").at;
    assert!(last <= Schedule::DEFAULT.give_up, "{last:?}");
    assert!(
        fake.clock <= Schedule::DEFAULT.give_up,
        "the clock ran to {:?}",
        fake.clock
    );
    assert_eq!(
        fake.reads.len(),
        41,
        "a read every 250 ms for 10 s, ends included"
    );
    assert!(report.found.mine.is_empty());
    assert_eq!(report.failed, None);
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
    let mut fake = Fake::new(1);
    fake.broken = Some("log is down");
    let report = fake.run(Duration::ZERO, Schedule::DEFAULT);
    assert_eq!(fake.reads.len(), 1);
    assert!(fake.pauses.is_empty(), "no pause after a failed read");
    assert_eq!(report.failed.as_deref(), Some("log is down"));
    let said = report.explain(&BTreeSet::from([123]));
    assert!(said.contains("the last read failed: log is down"), "{said}");
    // The query named is the one the failed read was given, not a window of none.
    assert!(said.contains("--last 10s --predicate"), "{said}");
    assert!(!said.contains("--last 1s "), "{said}");
}

/// Catches a read of a short action that looks back over minutes of the whole host's log
/// (a two-minute floor: on a busy runner that read took longer than a read may, and the
/// action's line was never looked for), one that ignores a long action or the wait since
/// its end, and a reader handed a different window than the report records.
#[test]
fn the_window_is_the_run_the_wait_and_the_margin_only() {
    assert_eq!(
        window(Duration::from_secs(1), Duration::ZERO),
        Duration::from_secs(11)
    );
    assert_eq!(
        window(Duration::from_millis(2_400), Duration::from_millis(300)),
        Duration::from_millis(12_700)
    );
    assert_eq!(
        window(Duration::from_secs(300), Duration::from_secs(5)),
        Duration::from_secs(315)
    );
    let mut fake = Fake::new(3);
    let report = fake.run(Duration::from_secs(200), schedule(10_000));
    assert_eq!(fake.reads[0], Duration::from_secs(210));
    assert_eq!(fake.reads[2], Duration::from_millis(210_500));
    assert_eq!(report.asked, Duration::from_millis(210_500));
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
    let mut one = Fake::new(usize::MAX);
    one.others = true;
    one.noise = "Sandbox: odd deny file-write-data /x\nnoise\n";
    let report = one.run(Duration::from_secs(1), schedule(0));
    let bytes = report.attempts[0].bytes;
    let said = report.explain(&BTreeSet::from([123, 6]));
    assert!(said.contains("watched pid(s) {6, 123}"), "{said}");
    assert!(said.contains("1 read(s)"), "{said}");
    assert!(said.contains("--last 11s --predicate"), "{said}");
    assert!(said.contains(PREDICATE), "{said}");
    assert!(said.contains(&format!("{bytes} byte(s)")), "{said}");
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

/// Lines of the log, oldest first, ending in the denial.
fn long_log(lines: usize) -> String {
    let mut log = String::new();
    for i in 0..lines {
        log.push_str(&format!("2026-10-17 10:00:00.1 Df noise line {i}\n"));
    }
    log.push_str(LINE);
    log.push('\n');
    log
}

/// Catches a read that keeps the first `cap` bytes of output longer than that (the
/// newest line, the denial, is the last `log show` prints), one that keeps a partial
/// first line, one that returns more than `cap`, and a short output not returned whole.
#[test]
fn output_over_the_cap_keeps_its_end_from_a_whole_line() {
    let log = long_log(500);
    let cap = 1000;
    assert!(log.len() as u64 > 10 * cap);
    let tail = read_tail(io::Cursor::new(log.as_bytes()), cap).expect("read");
    assert!(tail.len() as u64 <= cap, "{}", tail.len());
    assert!(tail.ends_with(&format!("{LINE}\n")), "{tail}");
    assert!(log.ends_with(&tail));
    assert!(
        tail.starts_with("2026-10-17 10:00:00.1 Df noise line "),
        "{tail}"
    );
    assert!(
        Found::of(&tail, &BTreeSet::from([123])).note().is_some(),
        "the denial is kept"
    );
    // A cut exactly at a line start keeps that line; output of `cap` bytes is whole.
    let two = format!("first\n{LINE}\n");
    let cap = (LINE.len() + 1) as u64;
    assert_eq!(
        read_tail(io::Cursor::new(two.as_bytes()), cap).expect("read"),
        format!("{LINE}\n")
    );
    assert_eq!(
        read_tail(io::Cursor::new(two.as_bytes()), two.len() as u64).expect("read"),
        two
    );
    assert_eq!(read_tail(io::Cursor::new(&b""[..]), 10).expect("read"), "");
}

/// Catches a read that fails when the cut falls inside a multibyte character (the
/// report would end with no denial), and one that keeps the damaged first line.
#[test]
fn a_character_split_by_the_cut_does_not_fail_the_read() {
    let log = format!("{}\n{LINE}\n", "\u{e9}".repeat(10));
    // Two bytes short of the whole: the window (one byte more than the cap) starts inside
    // the first e-acute.
    let cap = (log.len() - 2) as u64;
    assert!(std::str::from_utf8(&log.as_bytes()[log.len() - cap as usize - 1..]).is_err());
    let tail = read_tail(io::Cursor::new(log.as_bytes()), cap).expect("read");
    assert_eq!(tail, format!("{LINE}\n"));
    // Bytes that are not UTF-8 anywhere become replacement characters, and the rest stays.
    let mut raw = format!("{LINE}\n").into_bytes();
    raw.insert(0, 0xff);
    raw.push(b'\n');
    let all = read_tail(io::Cursor::new(raw), 1 << 20).expect("read");
    assert!(all.starts_with('\u{fffd}'), "{all}");
    assert!(all.contains(LINE), "{all}");
}

/// Catches a reader error swallowed into an empty log.
#[test]
fn a_source_that_cannot_be_read_is_an_error() {
    struct Bad;
    impl Read for Bad {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("disk"))
        }
    }
    impl Seek for Bad {
        fn seek(&mut self, _: io::SeekFrom) -> io::Result<u64> {
            Ok(100)
        }
    }
    assert_eq!(read_tail(Bad, 10).expect_err("fails").to_string(), "disk");
}

/// Catches a log reader off macOS that claims to read something.
#[cfg(not(target_os = "macos"))]
#[test]
fn off_macos_there_is_no_sandbox_log() {
    let why = system_log(Path::new("/nonexistent"), Duration::from_secs(1)).expect_err("no log");
    assert_eq!(why.kind(), io::ErrorKind::Unsupported);
}
