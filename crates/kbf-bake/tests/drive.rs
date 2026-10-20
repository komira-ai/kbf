//! The scrape loop over a scripted farm: the verdict file, the state file and the clock the
//! loop leaves behind.

mod common;

use common::*;
use kbf_bake::drive::{Fetched, drive};
use kbf_bake::state::State;
use kbf_gate::{Run, Status, Verdict};

const FOUR_HOURS: u64 = 4 * 3600;

/// A healthy bake with enough load runs to its end and is green; its verdict is whole.
#[test]
fn a_healthy_bake_with_load_ends_green_with_a_whole_verdict() {
    let bake = Bake::new("drive-green");
    let mut e = bake.evaluator(FOUR_HOURS);
    let out = bake.drive(&mut e, growing(10.0), Some(12), Some(quiet_log()));
    assert_eq!(out.status, Status::Green, "{}", out.note);
    let doc = bake.doc();
    assert!(doc.passed());
    assert_eq!((doc.started, doc.finished), (T0, Some(T0 + FOUR_HOURS)));
    let bake_window = doc.bake.unwrap();
    assert_eq!(
        (bake_window.start, bake_window.end),
        (T0, Some(T0 + FOUR_HOURS))
    );
    assert!(doc.load.unwrap().met());
    assert_eq!(doc.gates.len(), 11);
    assert!(doc.gates.iter().all(|g| g.verdict != Verdict::Red));
    assert_eq!(
        State::load(&bake.state).unwrap().unwrap().ended,
        Some(Status::Green)
    );
}

/// Catches a bake that passes on elapsed time alone: four quiet hours is `Extended`, not green,
/// the verdict says why, and the bake is not over (a later run goes on).
#[test]
fn thin_usage_extends_the_bake_instead_of_passing() {
    let bake = Bake::new("drive-thin");
    let mut e = bake.evaluator(FOUR_HOURS);
    let out = bake.drive(&mut e, growing(0.5), Some(12), Some(quiet_log()));
    assert_eq!(out.status, Status::Extended);
    assert!(
        out.note.contains("minimum load not met: actions"),
        "{}",
        out.note
    );
    let doc = bake.doc();
    assert_eq!(doc.status, Status::Extended);
    assert!(!doc.passed());
    assert_eq!(doc.note.as_deref(), Some(out.note.as_str()));
    assert_eq!(State::load(&bake.state).unwrap().unwrap().ended, None);
}

/// Catches a hard gate that waits for the end of the window: the lost blob at ten minutes
/// ends the run at ten minutes, red, with the bake's end recorded.
#[test]
fn a_hard_gate_ends_the_run_at_that_scrape() {
    let bake = Bake::new("drive-hard");
    let mut e = bake.evaluator(86_400);
    let out = bake.drive(
        &mut e,
        |t| {
            Ok(Farm {
                lost_blobs: if t >= T0 + 600 { 1.0 } else { 0.0 },
                ..Farm::healthy(1.0)
            })
        },
        Some(1),
        Some(quiet_log()),
    );
    assert_eq!(out.status, Status::Red);
    assert_eq!(now(), T0 + 600, "the loop did not wait out the day");
    let doc = bake.doc();
    assert_eq!(doc.status, Status::Red);
    assert_eq!(doc.bake.unwrap().end, Some(T0 + 600));
    assert!(doc.note.unwrap().contains("`lost-blobs` is red"));
    assert_eq!(
        State::load(&bake.state).unwrap().unwrap().ended,
        Some(Status::Red)
    );
}

/// Catches a verdict that stays `RUNNING` with no gates until the end: it is updated at each
/// scrape, so a killed evaluator leaves the last decision, still not a pass.
#[test]
fn the_verdict_is_running_with_gates_until_the_bake_ends() {
    let bake = Bake::new("drive-running");
    let mut e = bake.evaluator(FOUR_HOURS);
    let mut seen = Vec::new();
    let run = Run::start(&bake.verdict, Bake::header(), now).unwrap();
    let path = bake.verdict.clone();
    let mut calls = 0;
    drive(
        &mut e,
        run,
        &bake.state,
        INTERVAL,
        &mut || {
            calls += 1;
            if calls == 3 {
                seen.push(kbf_gate::ResultDoc::from_bytes(&std::fs::read(&path).unwrap()).unwrap());
            }
            Ok(Fetched {
                text: Farm::healthy(0.0).text(),
                builds: Some(0),
            })
        },
        &mut || Some(quiet_log()),
        &mut |s| set_now(now() + s),
    )
    .unwrap();
    let mid = &seen[0];
    assert_eq!(
        (mid.status, mid.finished, mid.started),
        (Status::Running, None, T0)
    );
    assert_eq!(mid.gates.len(), 11);
    assert_eq!(mid.bake.unwrap().end, None);
    assert!(!mid.passed());
}

/// Catches an evaluator that passes, or ends red, when the farm cannot be read: three reads in
/// a row that fail extend the bake, and the state is kept for the next run.
#[test]
fn a_farm_that_cannot_be_read_extends_the_bake_and_never_passes() {
    let bake = Bake::new("drive-down");
    let mut e = bake.evaluator(FOUR_HOURS);
    let mut tick = 0;
    let out = bake.drive(
        &mut e,
        |_| {
            tick += 1;
            if tick == 1 {
                Ok(Farm::healthy(0.0))
            } else {
                Err("connection refused".to_owned())
            }
        },
        Some(0),
        Some(quiet_log()),
    );
    assert_eq!(out.status, Status::Extended);
    assert!(
        out.note.contains("3 times in a row: connection refused"),
        "{}",
        out.note
    );
    assert_eq!(bake.doc().status, Status::Extended);
    let kept = State::load(&bake.state).unwrap().unwrap();
    assert_eq!((kept.bake_start, kept.ended), (T0, None));
}

/// Catches a failure count that is never reset: two failures, a good read, two more, and so on
/// never reach three in a row, and the bake runs to its end.
#[test]
fn failures_that_are_not_in_a_row_do_not_extend_the_bake() {
    let bake = Bake::new("drive-flaky");
    let mut e = bake.evaluator(FOUR_HOURS);
    e.waive(kbf_gate::Waiver::new("min-load", "operator", "rehearsal").unwrap());
    let mut ok = growing(10.0);
    let out = bake.drive(
        &mut e,
        |t| {
            if (t - T0) / INTERVAL % 3 == 2 || t == T0 {
                ok(t)
            } else {
                Err("timeout".to_owned())
            }
        },
        Some(12),
        Some(quiet_log()),
    );
    assert_eq!(out.status, Status::Green, "{}", out.note);
}

/// Catches text that is not a scrape being treated as a scrape: it is a failed read.
#[test]
fn a_body_that_is_not_metrics_is_a_failed_read() {
    let bake = Bake::new("drive-garbage");
    let mut e = bake.evaluator(FOUR_HOURS);
    let run = Run::start(&bake.verdict, Bake::header(), now).unwrap();
    let out = drive(
        &mut e,
        run,
        &bake.state,
        INTERVAL,
        &mut || {
            Ok(Fetched {
                text: "<html>502</html>\n".to_owned(),
                builds: None,
            })
        },
        &mut || None,
        &mut |s| set_now(now() + s),
    )
    .unwrap();
    assert_eq!(out.status, Status::Extended);
    assert!(out.note.contains("metrics line 1"), "{}", out.note);
}

/// Catches an evaluator error that falls out as a red verdict (the run's drop guard): the
/// state cannot be saved, and the verdict says `extended` with the error.
#[test]
fn an_evaluator_error_extends_the_bake_it_does_not_fail_it() {
    let bake = Bake::new("drive-error");
    let mut e = bake.evaluator(FOUR_HOURS);
    let run = Run::start(&bake.verdict, Bake::header(), now).unwrap();
    let no_such_dir = bake
        .state
        .parent()
        .unwrap()
        .join("missing")
        .join("state.json");
    let err = drive(
        &mut e,
        run,
        &no_such_dir,
        INTERVAL,
        &mut || {
            Ok(Fetched {
                text: Farm::healthy(0.0).text(),
                builds: None,
            })
        },
        &mut || None,
        &mut |s| set_now(now() + s),
    )
    .unwrap_err();
    assert!(err.to_string().contains("write state file"), "{err}");
    let doc = bake.doc();
    assert_eq!(doc.status, Status::Extended);
    assert!(doc.note.unwrap().contains("the evaluator failed"));
}

/// Catches a restart that starts the clock over: the first run is cut off by a dead farm, the
/// second reads the file, and the bake that finishes began at the first run's start.
#[test]
fn a_second_run_goes_on_from_the_state_of_the_first() {
    let bake = Bake::new("drive-resume");
    let mut e = bake.evaluator(FOUR_HOURS);
    let mut tick = 0;
    let first = bake.drive(
        &mut e,
        |t| {
            tick += 1;
            if tick <= 30 {
                growing(10.0)(t)
            } else {
                Err("down".to_owned())
            }
        },
        Some(12),
        Some(quiet_log()),
    );
    assert_eq!(first.status, Status::Extended);
    // The evaluator stays down for ten more minutes, and comes back.
    set_now(now() + 600);
    let mut e = bake.evaluator(FOUR_HOURS);
    assert_eq!(e.state().bake_start, T0);
    let second = bake.drive(&mut e, growing(10.0), Some(12), Some(quiet_log()));
    assert_eq!(second.status, Status::Green, "{}", second.note);
    assert!(
        bake.doc().finished.unwrap() > T0 + FOUR_HOURS,
        "the minutes nobody watched were added to the bake"
    );
    assert_eq!(
        bake.doc().started,
        T0,
        "the verdict begins at the bake's start, not the restart's"
    );
}
