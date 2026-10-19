//! The evaluator's decisions on scripted scrapes: each test names the defect it catches.
//! The farm is a canned metrics text; time is the observation's own field.

mod common;

use common::*;
use kbf_bake::eval::{Decision, End, Evaluator};
use kbf_bake::state::State;
use kbf_bake::updater::{Restart, Step, UpdaterLog};
use kbf_gate::{Status, Verdict, Waiver};

fn gate<'a>(d: &'a Decision, name: &str) -> &'a kbf_gate::GateResult {
    d.gates
        .iter()
        .find(|g| g.name == name)
        .unwrap_or_else(|| panic!("no gate {name}"))
}

fn ended(d: &Decision) -> &End {
    d.end
        .as_ref()
        .unwrap_or_else(|| panic!("the bake went on: {:?}", d.gates))
}

/// An evaluator that never counts a gap as blind time, for scrapes hours apart.
fn sparse(secs: u64) -> Evaluator {
    Evaluator::new(state(secs), u64::MAX)
}

fn with_state(secs: u64, change: impl FnOnce(&mut State)) -> Evaluator {
    let mut s = state(secs);
    change(&mut s);
    Evaluator::new(s, u64::MAX)
}

/// The first scrape of a bake: every counter is read, nothing has happened.
fn first(e: &mut Evaluator) {
    let d = observe(e, T0, &Farm::healthy(0.0), 0, Some(&quiet_log()));
    assert_eq!(d.end, None);
}

/// A bake of three hours whose last scrape is its end, with `actions` executed in
/// `active` of the hours and `builds` builds.
fn three_hour_bake(actions: u64, builds: u64, active: u64, waived: bool) -> End {
    let mut e = sparse(3 * 3600);
    if waived {
        e.waive(Waiver::new("min-load", "operator", "holiday week").unwrap());
    }
    first(&mut e);
    let log = quiet_log();
    let mut last = None;
    for h in 1..=3u64 {
        let cum = actions * h.min(active) / active;
        let d = observe(
            &mut e,
            T0 + h * 3600,
            &Farm::healthy(cum as f64),
            builds,
            Some(&log),
        );
        last = Some(d);
    }
    ended(&last.unwrap()).clone()
}

/// Catches a bake that passes on elapsed time alone, and an off-by-one in any of the three
/// floors: 1000 actions, 10 builds and 3 hours pass, one less of any extends.
#[test]
fn the_minimum_load_boundaries_are_exact() {
    let ok = three_hour_bake(1000, 10, 3, false);
    assert_eq!((ok.status, ok.note.as_str()), (Status::Green, ""));
    for (actions, builds, hours, what) in [
        (999, 10, 3, "actions 999/1000"),
        (1000, 9, 3, "builds 9/10"),
        (1000, 10, 2, "hours 2/3"),
    ] {
        let end = three_hour_bake(actions, builds, hours, false);
        assert_eq!(end.status, Status::Extended, "{what}");
        assert!(end.note.contains(what), "{}", end.note);
    }
}

/// Catches a waiver that is not honoured, and one that waives more than the load.
#[test]
fn a_recorded_waiver_lets_thin_load_pass_and_nothing_else() {
    assert_eq!(three_hour_bake(5, 0, 1, true).status, Status::Green);
    // A red gate is not waivable.
    let mut e = sparse(60);
    e.waive(Waiver::new("min-load", "operator", "holiday week").unwrap());
    first(&mut e);
    let d = observe(
        &mut e,
        T0 + 60,
        &Farm {
            lost_blobs: 1.0,
            ..Farm::healthy(5.0)
        },
        0,
        Some(&quiet_log()),
    );
    assert_eq!(ended(&d).status, Status::Red);
}

/// Catches an absent series read as zero or skipped: each gate whose series is missing is
/// red with no measurement, and a gate that always judges ends the bake at that scrape.
#[test]
fn an_absent_series_is_red_for_every_gate() {
    for (gate_name, family) in [
        ("farm-class-failures", "kbf_failed_actions_total"),
        ("lost-blobs", "kbf_cache_lost_blobs"),
        ("blob-losses-delta", "kbf_cache_blob_losses_total"),
        ("relay-untraced", "kbf_frontend_relay_untraced"),
        (
            "write-tier-flush-failures",
            "kbf_write_tier_flush_failures_total",
        ),
        (
            "write-tier-digest-mismatches",
            "kbf_tier_flush_digest_mismatch_total",
        ),
    ] {
        let mut e = evaluator(86_400);
        first(&mut e);
        let d = observe(
            &mut e,
            T0 + 60,
            &Farm::healthy(10.0).without(family),
            12,
            Some(&quiet_log()),
        );
        let g = gate(&d, gate_name);
        assert_eq!((g.verdict, g.measured), (Verdict::Red, None), "{gate_name}");
        let end = ended(&d);
        assert_eq!(end.status, Status::Red, "{gate_name}");
        assert!(end.note.contains(gate_name), "{}", end.note);
    }
    // The gates that judge a ratio wait for the load floor, and are red once it is met.
    for (gate_name, family) in [
        ("error-rate", "kbf_execute_errors_total"),
        ("latency-p99-ratio", "kbf_execute_queued_seconds"),
    ] {
        let mut e = evaluator(86_400);
        first(&mut e);
        let d = observe(
            &mut e,
            T0 + 60,
            &Farm::healthy(1000.0).without(family),
            12,
            Some(&quiet_log()),
        );
        assert_eq!(gate(&d, gate_name).verdict, Verdict::Red, "{gate_name}");
        assert_eq!(ended(&d).status, Status::Red, "{gate_name}");
    }
    // No updater log is no evidence about restarts.
    let mut e = evaluator(86_400);
    first(&mut e);
    let d = observe(&mut e, T0 + 60, &Farm::healthy(10.0), 12, None);
    assert_eq!(gate(&d, "restarts").verdict, Verdict::Red);
    assert_eq!(ended(&d).status, Status::Red);
}

/// Catches a bake that waits out the 24 hours after a hard gate failed: the second scrape,
/// a minute in, is already red, with the reason.
#[test]
fn a_hard_gate_ends_the_bake_at_once() {
    let mut e = evaluator(86_400);
    first(&mut e);
    let d = observe(
        &mut e,
        T0 + 60,
        &Farm {
            lost_blobs: 1.0,
            ..Farm::healthy(10.0)
        },
        12,
        Some(&quiet_log()),
    );
    let end = ended(&d);
    assert_eq!(end.status, Status::Red);
    assert!(end.note.contains("`lost-blobs` is red"), "{}", end.note);
    // The loss counter is a gate of its own: the delta since the bake began.
    let mut e = evaluator(86_400);
    first(&mut e);
    let d = observe(
        &mut e,
        T0 + 60,
        &Farm {
            losses: 1.0,
            ..Farm::healthy(10.0)
        },
        12,
        Some(&quiet_log()),
    );
    assert!(ended(&d).note.contains("`blob-losses-delta`"));
}

/// Catches a counter gate that compares the absolute value: a farm that already had 5 losses
/// when the bake began is fine, and a counter that fell (the farm restarted) and rose again
/// counts the rise.
#[test]
fn counters_are_judged_by_their_increase_and_a_reset_is_not_a_decrease() {
    let mut e = evaluator(86_400);
    let log = quiet_log();
    let at = |t: u64, losses: f64, e: &mut Evaluator| {
        observe(
            e,
            T0 + t,
            &Farm {
                losses,
                ..Farm::healthy(1.0)
            },
            1,
            Some(&log),
        )
    };
    assert_eq!(at(0, 5.0, &mut e).end, None);
    assert_eq!(at(60, 5.0, &mut e).end, None);
    // The farm restarts: the counter starts again at 0 and reaches 2: two losses.
    let d = at(120, 2.0, &mut e);
    assert_eq!(gate(&d, "blob-losses-delta").measured, Some(2.0));
    assert!(ended(&d).note.contains("blob-losses-delta"));
}

/// Catches a ratio gate that fires on the first scrape's handful of requests, a warning
/// that ends the bake, and a band edge off by one step.
#[test]
fn latency_is_a_warning_to_three_times_and_red_above() {
    let run = |le: &'static str| {
        let mut e = evaluator(86_400);
        let mut zero = Farm::healthy(0.0);
        zero.queue = vec![("0.5", 0.0), (le, 0.0), ("+Inf", 0.0)];
        observe(&mut e, T0, &zero, 0, Some(&quiet_log()));
        let mut f = Farm::healthy(1000.0);
        f.queue = vec![("0.5", 900.0), (le, 1000.0), ("+Inf", 1000.0)];
        observe(&mut e, T0 + 60, &f, 12, Some(&quiet_log()))
    };
    // The p99 is the upper bound of the bucket that holds it; the baseline is 0.5 s.
    let at_warn = run("0.75");
    assert_eq!(gate(&at_warn, "latency-p99-ratio").verdict, Verdict::Green);
    assert_eq!(at_warn.end, None);
    let warned = run("0.755");
    assert_eq!(gate(&warned, "latency-p99-ratio").verdict, Verdict::Warn);
    assert_eq!(warned.end, None, "a soft gate does not end the bake");
    let at_red = run("1.5");
    assert_eq!(gate(&at_red, "latency-p99-ratio").verdict, Verdict::Warn);
    assert_eq!(at_red.end, None);
    let red = run("1.505");
    assert_eq!(gate(&red, "latency-p99-ratio").verdict, Verdict::Red);
    assert_eq!(ended(&red).status, Status::Red);
    // Early in the bake a handful of slow calls decide nothing.
    let mut e = evaluator(86_400);
    let mut zero = Farm::healthy(0.0);
    zero.queue = vec![("0.5", 0.0), ("9", 0.0), ("+Inf", 0.0)];
    observe(&mut e, T0, &zero, 0, Some(&quiet_log()));
    let mut f = Farm::healthy(10.0);
    f.queue = vec![("0.5", 0.0), ("9", 10.0), ("+Inf", 10.0)];
    assert_eq!(
        observe(&mut e, T0 + 60, &f, 1, Some(&quiet_log())).end,
        None
    );
}

/// Catches the error-rate limit taken as only twice the baseline, or only the baseline plus
/// half a point: it is the larger of the two.
#[test]
fn the_error_rate_limit_is_the_larger_of_twice_and_plus_half_a_point() {
    let judge = |baseline: f64, errors: f64| {
        let mut e = with_state(86_400, |s| s.baseline.error_ratio = Some(baseline));
        first(&mut e);
        let f = Farm {
            errors,
            ..Farm::healthy(1000.0)
        };
        observe(&mut e, T0 + 60, &f, 12, Some(&quiet_log()))
    };
    // Baseline 0: the half point decides (5 in 1000 passes, 6 fails).
    assert_eq!(gate(&judge(0.0, 5.0), "error-rate").verdict, Verdict::Green);
    assert_eq!(gate(&judge(0.0, 6.0), "error-rate").verdict, Verdict::Red);
    // Baseline 10 %: twice decides (200 passes, 201 fails).
    assert_eq!(
        gate(&judge(0.1, 200.0), "error-rate").verdict,
        Verdict::Green
    );
    assert_eq!(gate(&judge(0.1, 201.0), "error-rate").verdict, Verdict::Red);
    assert_eq!(ended(&judge(0.1, 201.0)).status, Status::Red);
}

/// Catches a missing baseline passing its gates.
#[test]
fn no_baseline_fails_the_error_and_latency_gates() {
    let mut e = with_state(86_400, |s| s.baseline = Default::default());
    first(&mut e);
    let d = observe(
        &mut e,
        T0 + 60,
        &Farm::healthy(1000.0),
        12,
        Some(&quiet_log()),
    );
    assert_eq!(gate(&d, "error-rate").verdict, Verdict::Red);
    assert_eq!(gate(&d, "latency-p99-ratio").verdict, Verdict::Red);
    assert_eq!(ended(&d).status, Status::Red);
}

fn under(e: &mut Evaluator, k: u64, value: Option<f64>, log: &UpdaterLog) -> Decision {
    let f = Farm {
        under: value,
        ..Farm::healthy(1.0)
    };
    observe(e, T0 + 60 * k, &f, 1, Some(log))
}

/// Catches `>=` for `>`, a clock that ignores the node steps, and a stretch that survives a
/// zero: above 0 for 10 minutes passes, for 11 fails, a zero in between starts over.
#[test]
fn under_replicated_for_more_than_ten_minutes_outside_a_node_step_is_red() {
    let quiet = quiet_log();
    let mut e = evaluator(86_400);
    for k in 0..=10 {
        assert_eq!(under(&mut e, k, Some(2.0), &quiet).end, None, "minute {k}");
    }
    let d = under(&mut e, 11, Some(2.0), &quiet);
    assert_eq!(gate(&d, "under-replicated-secs").measured, Some(660.0));
    assert!(
        ended(&d).note.contains("`under-replicated-secs`"),
        "{}",
        ended(&d).note
    );

    // A zero starts the count over.
    let mut e = evaluator(86_400);
    for k in 0..10 {
        under(&mut e, k, Some(2.0), &quiet);
    }
    under(&mut e, 10, Some(0.0), &quiet);
    for k in 11..=20 {
        assert_eq!(under(&mut e, k, Some(2.0), &quiet).end, None, "minute {k}");
    }
    // An absent series changes nothing: it neither ends nor restarts the stretch.
    let mut e = evaluator(86_400);
    for k in 0..5 {
        under(&mut e, k, Some(2.0), &quiet);
    }
    under(&mut e, 5, None, &quiet);
    for k in 6..=10 {
        under(&mut e, k, Some(2.0), &quiet);
    }
    assert_eq!(
        ended(&under(&mut e, 11, Some(2.0), &quiet)).status,
        Status::Red
    );
}

/// Catches node-step windows that are not an input: the same 30 minutes above zero is fine
/// inside a node step and is red outside one.
#[test]
fn a_node_step_window_excuses_the_blobs_it_makes_under_replicated() {
    let in_step = UpdaterLog {
        steps: vec![Step {
            host: "node-1".into(),
            start: T0,
            end: Some(T0 + 60 * 30),
        }],
        restarts: vec![],
    };
    let mut e = evaluator(86_400);
    for k in 0..=30 {
        assert_eq!(
            under(&mut e, k, Some(1.0), &in_step).end,
            None,
            "minute {k}"
        );
    }
    // The step is over and the blobs are still under-replicated: the count starts there.
    for k in 31..=41 {
        assert_eq!(
            under(&mut e, k, Some(1.0), &in_step).end,
            None,
            "minute {k}"
        );
    }
    assert_eq!(
        ended(&under(&mut e, 42, Some(1.0), &in_step)).status,
        Status::Red
    );
    // A step with no end is still running.
    let open = UpdaterLog {
        steps: vec![Step {
            host: "node-1".into(),
            start: T0,
            end: None,
        }],
        restarts: vec![],
    };
    let mut e = evaluator(86_400);
    for k in 0..=30 {
        assert_eq!(under(&mut e, k, Some(1.0), &open).end, None);
    }
}

/// Catches an end of bake that does not look at where the series is now: above zero at the
/// end is red though it has only just risen, and an absent series at the end is red.
#[test]
fn under_replicated_must_be_zero_or_known_at_the_end() {
    for value in [Some(1.0), None] {
        let mut e = sparse(600);
        first(&mut e);
        let d = observe(
            &mut e,
            T0 + 600,
            &Farm {
                under: value,
                ..Farm::healthy(1.0)
            },
            1,
            Some(&quiet_log()),
        );
        assert_eq!(
            gate(&d, "under-replicated").verdict,
            Verdict::Red,
            "{value:?}"
        );
        assert_eq!(ended(&d).status, Status::Red, "{value:?}");
    }
}

/// Catches a restarts gate blind to the updater's log, and one that counts the deploy's own
/// restarts from before the bake.
#[test]
fn a_restart_during_the_bake_is_red_and_the_deploys_own_is_not() {
    let before = UpdaterLog {
        steps: vec![],
        restarts: vec![Restart {
            host: "node-1".into(),
            at: T0 - 10,
        }],
    };
    let mut e = evaluator(86_400);
    first(&mut e);
    let d = observe(&mut e, T0 + 60, &Farm::healthy(1.0), 1, Some(&before));
    assert_eq!(d.end, None);
    let during = UpdaterLog {
        steps: vec![],
        restarts: vec![Restart {
            host: "node-2".into(),
            at: T0 + 30,
        }],
    };
    let d = observe(&mut e, T0 + 120, &Farm::healthy(1.0), 1, Some(&during));
    assert_eq!(gate(&d, "restarts").measured, Some(1.0));
    assert!(ended(&d).note.contains("`restarts`"));
}

/// Catches time nobody watched counting toward the 24 hours: a two hour gap between scrapes
/// extends a one hour bake by two hours.
#[test]
fn time_the_evaluator_did_not_watch_extends_the_bake() {
    let mut e = Evaluator::new(state(3600), 180);
    e.waive(Waiver::new("min-load", "operator", "rehearsal").unwrap());
    first(&mut e);
    let log = quiet_log();
    let d = observe(&mut e, T0 + 7200, &Farm::healthy(0.0), 0, Some(&log));
    assert_eq!(d.end, None, "two hours passed, one of them watched at most");
    assert_eq!(e.state().blind_secs, 7200);
    for k in 1..60 {
        let d = observe(
            &mut e,
            T0 + 7200 + 60 * k,
            &Farm::healthy(0.0),
            0,
            Some(&log),
        );
        assert_eq!(d.end, None, "{k} minutes after the gap");
    }
    let d = observe(&mut e, T0 + 7200 + 3600, &Farm::healthy(0.0), 0, Some(&log));
    assert_eq!(ended(&d).status, Status::Green);
}

/// Catches counters, hours and the clock that a restart forgets: the state read back from the
/// file carries the bake on, and a loss that happened across the restart is still counted.
#[test]
fn a_restart_keeps_the_clock_and_the_counters() {
    let bake = Bake::new("restart-keeps");
    let mut e = bake.evaluator(86_400);
    first(&mut e);
    observe(
        &mut e,
        T0 + 60,
        &Farm::healthy(500.0),
        3,
        Some(&quiet_log()),
    );
    e.state().save(&bake.state).unwrap();
    let before = e.state().clone();

    set_now(T0 + 3 * 3600);
    let mut e = bake.evaluator(86_400);
    assert_eq!(e.state(), &before);
    assert_eq!(e.state().bake_start, T0, "the clock is the first run's");
    let d = observe(
        &mut e,
        T0 + 120,
        &Farm {
            losses: 1.0,
            ..Farm::healthy(500.0)
        },
        3,
        Some(&quiet_log()),
    );
    assert_eq!(gate(&d, "blob-losses-delta").measured, Some(1.0));
    assert!(ended(&d).note.contains("blob-losses-delta"));
}

/// Catches a state file that lets a waiver without a reason in by the back door.
#[test]
fn a_state_file_with_a_waiver_without_a_reason_is_refused() {
    let bake = Bake::new("state-waiver");
    let mut s = state(60);
    s.waivers.push(Waiver {
        what: "min-load".into(),
        by: "operator".into(),
        reason: " ".into(),
    });
    s.save(&bake.state).unwrap();
    let err = State::load(&bake.state).unwrap_err().to_string();
    assert!(err.contains("names no operator or no reason"), "{err}");
    let mut s = state(60);
    s.schema = 2;
    s.save(&bake.state).unwrap();
    assert!(
        State::load(&bake.state)
            .unwrap_err()
            .to_string()
            .contains("schema 2")
    );
    std::fs::write(&bake.state, b"not json").unwrap();
    assert!(
        State::load(&bake.state)
            .unwrap_err()
            .to_string()
            .contains("not a bake state")
    );
}

/// Catches a zero-failure gate wired to the wrong series, or to the wrong classes: each series
/// moving by one ends the bake red naming its own gate, and a user's failing action is not the
/// farm's failure.
#[test]
fn each_zero_gate_reads_its_own_series() {
    type Break = fn(&mut Farm);
    let cases: [(&str, Break); 7] = [
        ("farm-class-failures", |f| f.failed_infra = 1.0),
        ("lost-blobs", |f| f.lost_blobs = 1.0),
        ("blob-losses-delta", |f| f.losses = 1.0),
        ("relay-untraced", |f| f.untraced = 1.0),
        ("write-tier-flush-failures", |f| f.flush_failures = 1.0),
        ("write-tier-digest-mismatches", |f| f.mismatches = 1.0),
        ("farm-class-failures", |f| f.failed_lost = 1.0),
    ];
    for (name, change) in cases {
        let mut e = evaluator(86_400);
        first(&mut e);
        let mut farm = Farm::healthy(10.0);
        change(&mut farm);
        let d = observe(&mut e, T0 + 60, &farm, 12, Some(&quiet_log()));
        assert!(
            ended(&d).note.contains(&format!("`{name}` is red")),
            "{name}: {}",
            ended(&d).note
        );
        let also: Vec<&str> = d
            .gates
            .iter()
            .filter(|g| g.name != name && g.verdict == Verdict::Red)
            .map(|g| g.name.as_str())
            .collect();
        assert!(also.is_empty(), "{name}: {also:?} are red as well");
    }
    // The user's own failures are not the farm's.
    let mut e = evaluator(86_400);
    first(&mut e);
    let d = observe(
        &mut e,
        T0 + 60,
        &Farm {
            failed_user: 500.0,
            ..Farm::healthy(10.0)
        },
        12,
        Some(&quiet_log()),
    );
    assert_eq!(d.end, None);
}

/// Catches a waiver recorded twice when the operator repeats the flag on a restart.
#[test]
fn the_same_waiver_is_recorded_once() {
    let mut e = evaluator(60);
    for reason in ["holiday week", "again"] {
        e.waive(Waiver::new("min-load", "operator", reason).unwrap());
    }
    assert_eq!(e.state().waivers.len(), 1);
    assert_eq!(e.state().waivers[0].reason, "holiday week");
    e.waive(Waiver::new("something-else", "operator", "x").unwrap());
    assert_eq!(e.state().waivers.len(), 2);
}

/// Catches a node step whose edges are off: both ends are inside, one second either side is
/// outside, and a step with no end runs from its start on.
#[test]
fn a_node_step_is_a_closed_window() {
    let log = UpdaterLog {
        steps: vec![Step {
            host: "a".into(),
            start: 100,
            end: Some(200),
        }],
        restarts: vec![],
    };
    let inside: Vec<bool> = [99, 100, 150, 200, 201].map(|t| log.in_step(t)).to_vec();
    assert_eq!(inside, [false, true, true, true, false]);
    let open = UpdaterLog {
        steps: vec![Step {
            host: "a".into(),
            start: 100,
            end: None,
        }],
        restarts: vec![],
    };
    assert_eq!(
        [99, 100, 9_999].map(|t| open.in_step(t)),
        [false, true, true]
    );
    assert!(
        UpdaterLog::parse(b"{\"steps\": 3}")
            .unwrap_err()
            .to_string()
            .starts_with("updater log:")
    );
    assert!(UpdaterLog::parse(br#"{"steps":[],"restarts":[],"extra":1}"#).is_err());
}

/// Catches a latency gate that passes on a farm that never exposed the histogram.
#[test]
fn a_farm_without_the_queue_histogram_from_the_start_fails_the_latency_gate() {
    let mut e = evaluator(86_400);
    let none = Farm::healthy(0.0).without("kbf_execute_queued_seconds");
    observe(&mut e, T0, &none, 0, Some(&quiet_log()));
    let d = observe(
        &mut e,
        T0 + 60,
        &Farm::healthy(1000.0).without("kbf_execute_queued_seconds"),
        12,
        Some(&quiet_log()),
    );
    assert_eq!(gate(&d, "latency-p99-ratio").verdict, Verdict::Red);
    assert_eq!(gate(&d, "latency-p99-ratio").measured, None);
}

/// Catches a state file that cannot be read being taken for no state file: a directory where
/// the file should be is an error, not a new bake.
#[test]
fn a_state_path_that_cannot_be_read_is_an_error_not_a_new_bake() {
    let dir = scratch("state-unreadable");
    let err = State::load(&dir).unwrap_err().to_string();
    assert!(err.starts_with("read state file"), "{err}");
}
