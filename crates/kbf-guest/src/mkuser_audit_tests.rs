//! Tests of the log audit: what the frame may hold (counts and an exit status, never log
//! text or the password), how the counts are made (exact and windowed, across reads), what
//! a failing `log show` reports, and that the audit never touches the ready marker. On the
//! fake host of `mkuser_tests`; nothing waits on a clock.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::Value;

use crate::mkuser::{plan, step_frame_name};
use crate::mkuser_audit::*;
use crate::mkuser_tests::{Call, Fake, PASSWORD, go};
use crate::sim::{Ended, Status};
use crate::simpaths;

/// Text that stands for log content; it must never reach a frame.
const LOG_TEXT: &str = "opendirectoryd secret-log-words";

fn entry(rest: &str) -> String {
    format!("2026-10-26 10:00:00.123 Df {rest}\n")
}

/// Scripts the fake `log show`: the predicate pass and the full pass.
fn script(host: &Fake, matched: (Ended, &[&str]), full: (Ended, &[&str])) {
    let chunks = |c: &[&str]| c.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
    let mut s = host.streams.borrow_mut();
    s.push(("--predicate".to_owned(), matched.0, chunks(matched.1)));
    s.push(("--debug".to_owned(), full.0, chunks(full.1)));
}

fn audit_frame(host: &Fake) -> (String, AuditFrame) {
    let sent = host.sent.borrow();
    let f = sent
        .iter()
        .find(|f| f.name == AUDIT_NAME)
        .expect("an audit frame");
    (
        f.body.clone(),
        serde_json::from_str(&f.body).expect("audit json"),
    )
}

fn run_audit(full: &[&str]) -> AuditFrame {
    let host = Fake::new();
    script(&host, (Ended::Exited(0), &[]), (Ended::Exited(0), full));
    audit(&host, PASSWORD, Duration::from_secs(1))
}

/// Catches an audit that sends the log (a line, the password, a piece of it, a note), adds
/// a key beyond the counts and the exit status, or captures a step's output: the fake log
/// output holds the password and text, and no frame may hold either; the log is read only
/// as a stream, never as a command whose stdout and stderr a frame keeps.
#[test]
fn the_frame_holds_only_counts_and_an_exit_status_never_log_text() {
    let host = Fake::new();
    let hit = entry(&format!("sysadminctl[9]: -password {PASSWORD} {LOG_TEXT}"));
    script(
        &host,
        (Ended::Exited(0), &[&hit]),
        (Ended::Exited(0), &[&hit, &hit]),
    );
    let done = go(&host);
    assert!(done.summary.ok);
    let (body, frame) = audit_frame(&host);
    let keys: BTreeSet<String> = serde_json::from_str::<Value>(&body)
        .expect("json")
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    let want: BTreeSet<String> = [
        "step",
        "status",
        "exit",
        "signal",
        "matched_lines",
        "password_hits",
        "window_hits",
    ]
    .map(str::to_owned)
    .into();
    assert_eq!(keys, want);
    assert_eq!(
        (frame.step.as_str(), frame.status, frame.exit, frame.signal),
        (STEP, Status::Ok, Some(0), None)
    );
    assert_eq!(
        frame.counts,
        Counts {
            matched_lines: 1,
            password_hits: 2,
            window_hits: 2
        }
    );
    // No frame of the run, whatever its kind, holds the log's text or any piece of the
    // password beyond what the mkuser frames already hold (none).
    let pieces: Vec<&str> = (0..=PASSWORD.len() - WINDOW)
        .map(|i| &PASSWORD[i..i + WINDOW])
        .collect();
    for f in host.sent.borrow().iter() {
        assert!(!f.body.contains(LOG_TEXT), "{}", f.name);
        assert!(!f.body.contains("sysadminctl[9]"), "{}", f.name);
        assert!(!f.body.contains("2026-10-26"), "{}", f.name);
        for p in &pieces {
            assert!(!f.body.contains(p), "{} holds {p}", f.name);
        }
    }
    // Streamed, not run: the two `log show` commands are not commands a step frame captures.
    let streams: Vec<Vec<String>> = host
        .calls
        .borrow()
        .iter()
        .filter_map(|c| match c {
            Call::Stream(a) => Some(a.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(streams, [matched_argv(), full_argv()]);
    assert!(host.commands().iter().all(|c| !c.contains("/usr/bin/log")));
}

/// Catches a changed command: the window of time, the debug and info levels, the style and
/// the predicate are the question being asked.
#[test]
fn the_two_passes_ask_for_the_last_hour_with_the_predicate_and_without() {
    assert_eq!(
        matched_argv(),
        [
            "/usr/bin/log",
            "show",
            "--last",
            "1h",
            "--info",
            "--debug",
            "--style",
            "compact",
            "--predicate",
            "process == \"sysadminctl\" OR process == \"dscl\" OR eventMessage CONTAINS \"-password\" OR eventMessage CONTAINS[c] \"kcpassword\""
        ]
    );
    assert_eq!(
        full_argv(),
        ["/usr/bin/log", "show", "--last", "1h", "--info", "--debug"]
    );
}

/// Catches counts that miss a password split between two reads or at the end of the output
/// (no newline), that count a 7-character piece, a header or a continuation line, that
/// count a line twice for two pieces, or that take a window hit for an exact one.
#[test]
fn counts_are_exact_and_windowed_across_reads() {
    let (a, b) = PASSWORD.split_at(9);
    let first = entry(&format!("x {PASSWORD} y"));
    let window_only = entry(&format!("x {} y", &PASSWORD[3..11]));
    let seven = entry(&format!("x {} y", &PASSWORD[3..10]));
    let two_pieces = entry(&format!("x {} {} y", &PASSWORD[0..8], &PASSWORD[14..22]));
    let split = format!("2026-10-26 10:00:00.1 Df s {a}");
    let split_rest = format!("{b} t\n");
    let last = format!("tail line {PASSWORD}");
    let f = run_audit(&[
        &first,
        &window_only,
        &seven,
        &two_pieces,
        &split,
        &split_rest,
        "\n",
        &last,
    ]);
    // exact: first, split, last. window: those three and window_only and two_pieces.
    assert_eq!(f.counts.password_hits, 3);
    assert_eq!(f.counts.window_hits, 5);
    assert_eq!(f.counts.matched_lines, 0, "the first pass printed nothing");
}

/// Catches an entry count that includes the header lines, continuation lines or blank lines
/// of `log show`, or that misses an entry split over reads.
#[test]
fn matched_lines_counts_entries_only() {
    let host = Fake::new();
    let one = entry("dscl[1]: a");
    let split_a = "2026-10-26 10:00:";
    let split_b = "00.5 Df opendirectoryd: b\n";
    script(
        &host,
        (
            Ended::Exited(0),
            &[
                "Timestamp               Thr     Type     Activity   PID    TTL\n",
                "Filtering the log data using \"process == \\\"dscl\\\"\"\n",
                &one,
                "\n",
                "  continuation of an entry\n",
                split_a,
                split_b,
                "2026-10-26",
            ],
        ),
        (Ended::Exited(0), &[]),
    );
    let f = audit(&host, PASSWORD, Duration::from_secs(1));
    assert_eq!(f.counts.matched_lines, 2);
    assert_eq!(f.counts.password_hits, 0);
}

/// Catches a password hit taken from the predicate pass (the question is about the whole
/// log), and a hit count that grows from the first pass's text.
#[test]
fn password_hits_come_from_the_full_pass_only() {
    let host = Fake::new();
    let hit = entry(&format!("sysadminctl: {PASSWORD}"));
    script(
        &host,
        (Ended::Exited(0), &[&hit]),
        (Ended::Exited(0), &[&entry("nothing here")]),
    );
    let f = audit(&host, PASSWORD, Duration::from_secs(1));
    assert_eq!(
        f.counts,
        Counts {
            matched_lines: 1,
            password_hits: 0,
            window_hits: 0
        }
    );
}

/// Catches a scanner that breaks on a line longer than any read, a password shorter than a
/// window (the whole password is then the piece) or an empty one (which would match every
/// line).
#[test]
fn short_empty_and_very_long_lines_are_handled() {
    let host = Fake::new();
    let long = "z".repeat(100_000);
    script(
        &host,
        (Ended::Exited(0), &[]),
        (
            Ended::Exited(0),
            &[&long, PASSWORD, &long, "\nab", "c\nabc abc\n"],
        ),
    );
    let f = audit(&host, PASSWORD, Duration::from_secs(1));
    assert_eq!((f.counts.password_hits, f.counts.window_hits), (1, 1));
    // A 3-character password: the piece is the whole password; one line holds it split, one is a long line, one holds it twice.
    let f = audit(&host, "abc", Duration::from_secs(1));
    assert_eq!((f.counts.password_hits, f.counts.window_hits), (3, 3));
    // An empty password finds nothing.
    let f = audit(&host, "", Duration::from_secs(1));
    assert_eq!((f.counts.password_hits, f.counts.window_hits), (0, 0));
}

/// Catches an audit that hides a failed `log show` as success, loses its exit code or
/// signal, or reports it with text. Each way a command can end gives its status, and the
/// frame holds no text from it (the fake prints log text before failing).
#[test]
fn a_failed_log_show_is_reported_by_status_and_exit_without_text() {
    let cases = [
        (Ended::Exited(64), Status::Failed, Some(64), None),
        (Ended::Signaled(9), Status::Failed, None, Some(9)),
        (Ended::TimedOut, Status::Timeout, None, None),
        (
            Ended::SpawnFailed(format!("/usr/bin/log: {LOG_TEXT}")),
            Status::Error,
            None,
            None,
        ),
    ];
    for (ended, status, exit, signal) in cases {
        for fail_first in [true, false] {
            let host = Fake::new();
            let hit = entry(&format!("{LOG_TEXT} {PASSWORD}"));
            let (m, f) = if fail_first {
                (
                    (ended.clone(), &[hit.as_str()][..]),
                    (Ended::Exited(0), &[][..]),
                )
            } else {
                (
                    (Ended::Exited(0), &[][..]),
                    (ended.clone(), &[hit.as_str()][..]),
                )
            };
            script(&host, m, f);
            let done = go(&host);
            let (body, frame) = audit_frame(&host);
            assert_eq!(
                (frame.status, frame.exit, frame.signal),
                (status, exit, signal),
                "{ended:?} first={fail_first}"
            );
            assert!(
                !body.contains(LOG_TEXT) && !body.contains(PASSWORD),
                "{body}"
            );
            // Both passes ran whatever the first did.
            assert_eq!(
                host.calls
                    .borrow()
                    .iter()
                    .filter(|c| matches!(c, Call::Stream(_)))
                    .count(),
                2
            );
            assert!(
                done.summary.ok,
                "an audit failure is not a failure of the user"
            );
        }
    }
}

/// Catches an audit that gates, delays or fails the ready marker or the user: with a
/// failing `log show` the marker is still written, after the audit frame, the summary is
/// ok and the audit is not among its steps; and with a step that failed, the audit still
/// ran and the marker was not written.
#[test]
fn the_audit_runs_before_the_marker_and_never_gates_it() {
    let host = Fake::new();
    script(
        &host,
        (Ended::SpawnFailed("no log".to_owned()), &[]),
        (Ended::Exited(1), &[]),
    );
    let done = go(&host);
    assert!(done.summary.ok);
    assert_eq!(done.power_off_started, Some(true));
    assert!(
        host.writes()
            .iter()
            .any(|(p, b, _)| p == simpaths::USER_READY && b == b"kbfuser\n")
    );
    assert!(done.summary.steps.iter().all(|(id, _)| id != STEP));
    let order: Vec<String> = host
        .calls
        .borrow()
        .iter()
        .filter_map(|c| match c {
            Call::Send(n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let at = |name: &str| order.iter().position(|n| n == name).expect(name);
    let marker = step_frame_name(plan("kbfuser", false).len() - 1, "ready-marker");
    assert_eq!(at(&marker) + 1, at(crate::mkuser::SUMMARY_NAME));
    // The audit is sent right before the marker's frame, so after every other step's.
    assert_eq!(at(AUDIT_NAME) + 1, at(&marker), "{order:?}");

    let host = Fake::new();
    host.answer(
        "sysadminctl",
        crate::mkuser_tests::out(Ended::Exited(1), "", ""),
    );
    let done = go(&host);
    assert!(!done.summary.ok);
    assert_eq!(audit_frame(&host).1.status, Status::Ok);
    assert!(
        host.writes()
            .iter()
            .all(|(p, _, _)| p != simpaths::USER_READY)
    );
}

/// Catches an audit run when the user was ready already (nothing was done, no password was
/// made) or when the run could not start.
#[test]
fn no_audit_without_a_password() {
    let mut host = Fake::new();
    host.ready = Some("kbfuser".to_owned());
    let done = go(&host);
    assert!(done.summary.already_ready);
    assert!(!host.sent.borrow().iter().any(|f| f.name == AUDIT_NAME));
    assert!(
        !host
            .calls
            .borrow()
            .iter()
            .any(|c| matches!(c, Call::Stream(_)))
    );
}
