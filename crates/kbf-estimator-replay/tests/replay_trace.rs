//! From exported rows to operations: which rows are dropped and why, how attempts chain
//! into an operation, what need the record supports, and the null arm against the record.

mod common;

use std::collections::BTreeMap;

use common::*;
use kbf_estimator::memory_max;
use kbf_estimator_replay::cli::write_days;
use kbf_estimator_replay::replay::sim::simulate;
use kbf_estimator_replay::replay::stats::fidelity;
use kbf_estimator_replay::replay::trace::{build, load};
use kbf_estimator_replay::replay::{Arm, Params};
use kbf_estimator_replay::row::Row;
use kbf_testkit::dir::TestDir;

const DATA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/synthetic");

fn params() -> Params {
    Params::default()
}

/// Catches a chain read as separate operations (a ladder climb counted as three arrivals),
/// a need that ignores the kills before it, a booking taken from the wrong attempt, and a
/// rerun's queue time taken for an arrival.
#[test]
fn a_ladder_climb_is_one_operation() {
    let rows = vec![
        kill("d1", 100, 30, 4 * GIB, Some(16 * GIB)),
        kill("d1", 132, 40, 0, Some(32 * GIB)),
        attempt("d1", 175, 60, "ok", Some(30 * GIB)),
    ];
    let mut rows = rows;
    rows[1].booked_memory_bytes = None; // the record keeps no booking for this kill
    let t = build(rows, &params());
    assert_eq!(t.ops.len(), 1);
    let op = &t.ops[0];
    assert_eq!(op.first_booked, 4 * GIB);
    assert_eq!(op.recorded_kills, 2);
    assert_eq!(op.recorded_attempts, 3);
    assert_eq!(
        op.arrives_us, 0,
        "arrival is the first attempt's, shifted to 0"
    );
    assert_eq!(op.need, 30 * GIB, "the completing run's peak");
    assert_eq!(op.run_us, 60_000_000);
    assert_eq!(op.kill_us, 35_000_000, "the mean of the kills' times");
    assert_eq!(t.recorded.kills, 2);
    assert_eq!(t.funnel.kept, 3);
}

/// Catches a need below the kill that preceded it, and a final kill (the operation failed
/// at the cap) given a need it would have survived.
#[test]
fn the_need_is_at_least_one_byte_past_the_last_kill() {
    // Completed at 16 GiB with a recorded peak below the 4 GiB kill's limit (a run that
    // does not need the same each time): the need still exceeds that limit.
    let rows = vec![
        kill("d1", 0, 10, 4 * GIB, Some(16 * GIB)),
        attempt("d1", 12, 10, "ok", Some(GIB)),
    ];
    let t = build(rows, &params());
    assert_eq!(t.ops[0].need, memory_max(4 * GIB).unwrap() + 1);

    // A kill with no rerun: killed at the cap.
    let t = build(vec![kill("d2", 0, 10, 64 * GIB, None)], &params());
    assert_eq!(t.ops[0].need, memory_max(64 * GIB).unwrap() + 1);
    assert_eq!(t.ops[0].run_us, 10_000_000);
}

/// Catches attempts of one digest merged when they are separate submissions (a gap), and
/// not merged when the record names the operation (an operation id beats the gap), and
/// merged by an id that differs.
#[test]
fn chains_follow_the_gap_or_the_operation_id() {
    // A kill, and an ok run an hour later: two operations.
    let rows = vec![
        kill("d1", 0, 10, 4 * GIB, Some(16 * GIB)),
        attempt("d1", 4_000, 10, "ok", Some(GIB)),
    ];
    assert_eq!(build(rows, &params()).ops.len(), 2);

    // The same, with one operation id on both: one operation.
    let mut rows = vec![
        kill("d1", 0, 10, 4 * GIB, Some(16 * GIB)),
        attempt("d1", 4_000, 10, "ok", Some(GIB)),
    ];
    rows.iter_mut()
        .for_each(|r| r.operation = Some("op-1".into()));
    assert_eq!(build(rows, &params()).ops.len(), 1);

    // Adjacent in time but two operation ids: two.
    let mut rows = vec![
        kill("d1", 0, 10, 4 * GIB, Some(16 * GIB)),
        attempt("d1", 12, 10, "ok", Some(GIB)),
    ];
    rows[0].operation = Some("op-1".into());
    rows[1].operation = Some("op-2".into());
    assert_eq!(build(rows, &params()).ops.len(), 2);

    // A kill the ladder did not retry is not continued; nor are other digests.
    let rows = vec![
        kill("d1", 0, 10, 64 * GIB, None),
        attempt("d1", 12, 10, "ok", Some(GIB)),
        attempt("d2", 12, 10, "ok", Some(GIB)),
    ];
    assert_eq!(build(rows, &params()).ops.len(), 3);

    // Two runs of one digest back to back (the first completed): two operations.
    let rows = vec![
        attempt("d1", 0, 10, "ok", Some(GIB)),
        attempt("d1", 12, 10, "ok", Some(GIB)),
    ];
    assert_eq!(build(rows, &params()).ops.len(), 2);

    // A rerun that was queued before the kill ended is not its rerun.
    let rows = vec![
        kill("d1", 0, 100, 4 * GIB, Some(16 * GIB)),
        attempt("d1", 50, 10, "ok", Some(GIB)),
    ];
    assert_eq!(build(rows, &params()).ops.len(), 2);
}

/// Catches a node's capacity lost when the row that carries it is not the first: any row
/// of a node gives its capacity to all of them.
#[test]
fn a_node_capacity_from_any_of_its_rows_serves_them_all() {
    let mut bare = attempt("a", 0, 5, "ok", Some(GIB));
    bare.node_capacity = None;
    let t = build(vec![bare, attempt("b", 10, 5, "ok", Some(GIB))], &params());
    assert_eq!(t.ops.len(), 2, "the bare row is kept");
    assert_eq!(t.nodes.len(), 1);
    assert!(t.funnel.dropped.is_empty());
}

/// Catches rows that enter the replay when the model cannot place them, and a funnel that
/// does not say why.
#[test]
fn every_dropped_row_says_why() {
    let mut never = attempt("a", 0, 5, "lost", None);
    never.started_us = None;
    let mut pressure = kill("b", 0, 5, 4 * GIB, None);
    pressure.memory_kill = "node_pressure".into();
    let mut mac = attempt("e", 0, 5, "ok", Some(GIB));
    mac.platform = "macos".into();
    let mut nocap = attempt("f", 0, 5, "ok", Some(GIB));
    nocap.node = "other".into();
    nocap.node_capacity = None;
    let rows = vec![
        never,
        pressure,
        attempt("c", 0, 5, "timed_out", None),
        attempt("d", 0, 5, "infra", None),
        attempt("g", 0, 5, "lost", None),
        attempt("h", 0, 5, "cancelled", None),
        attempt("i", 0, 5, "mystery", None),
        mac,
        nocap,
        attempt("kept-ok", 0, 5, "ok", Some(GIB)),
        attempt("kept-failed", 0, 5, "failed", None),
    ];
    let t = build(rows, &params());
    let want: BTreeMap<&str, u64> = [
        ("never_ran", 1),
        ("node_pressure_kill", 1),
        ("timed_out", 1),
        ("infra", 1),
        ("lost", 1),
        ("cancelled", 1),
        ("unknown_outcome", 1),
        ("not_linux", 1),
        ("no_node_capacity", 1),
    ]
    .into();
    assert_eq!(t.funnel.dropped, want);
    assert_eq!((t.funnel.rows, t.funnel.kept), (11, 2));
    assert_eq!(t.ops.len(), 2);
    assert_eq!(t.funnel.no_peak, 1, "the failed run has no peak");
    assert_eq!(t.nodes.keys().collect::<Vec<_>>(), ["n1"]);
}

/// Catches a shape and configuration read wrongly: no shape means no key, and a
/// configuration only makes an L1 key.
#[test]
fn the_key_needs_a_shape_and_takes_the_configuration() {
    let mut keyless = attempt("a", 0, 5, "ok", Some(GIB));
    keyless.shape = "-".into();
    let mut zero = attempt("b", 0, 5, "ok", Some(GIB));
    zero.shape = "0000000000000000".into();
    let mut cfg = attempt("c", 0, 5, "ok", Some(GIB));
    cfg.cfg = "debug".into();
    let plain = attempt("d", 0, 5, "ok", Some(GIB));
    let t = build(vec![keyless, zero, cfg, plain], &params());
    assert_eq!(t.funnel.no_shape, 2);
    let keys: Vec<_> = t.ops.iter().map(|o| o.key.is_some()).collect();
    assert_eq!(keys.iter().filter(|k| **k).count(), 2);
    let with_cfg = t.ops.iter().find(|o| o.digest == "c").unwrap();
    let without = t.ops.iter().find(|o| o.digest == "d").unwrap();
    assert_ne!(with_cfg.key, without.key);
    assert_eq!(
        with_cfg.key.as_ref().unwrap().l2(),
        without.key.clone().unwrap()
    );
}

/// Catches the reader: files in name order, only `.jsonl`, one file by name, and a bad
/// line named with its file and line number.
#[test]
fn a_directory_or_a_file_loads() {
    let tmp = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "replay-load");
    let rows = vec![
        attempt("d1", 0, 5, "ok", Some(GIB)),
        attempt("d2", DAY_S, 5, "ok", Some(GIB)),
    ];
    write_days(&rows, &tmp).unwrap();
    std::fs::write(tmp.join("notes.txt"), "not a row").unwrap();
    let t = load(&tmp, &params()).unwrap();
    assert_eq!(
        t.ops.iter().map(|o| o.digest.as_str()).collect::<Vec<_>>(),
        ["d1", "d2"]
    );
    let one = load(&tmp.join("runs-2026-10-02.jsonl"), &params()).unwrap();
    assert_eq!(one.ops.len(), 1);

    std::fs::write(
        tmp.join("runs-2026-10-03.jsonl"),
        format!("{}\n{{\"oops\":1}}\n", rows[0].line()),
    )
    .unwrap();
    let err = load(&tmp, &params()).unwrap_err();
    assert!(
        err.to_string().contains("runs-2026-10-03.jsonl:2:"),
        "{err}"
    );
    let err = load(&tmp.join("missing"), &params()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// The synthetic trace on disk is what the generator makes. `BLESS=1` rewrites it.
#[test]
fn the_checked_in_trace_is_what_the_generator_makes() {
    let rows = synthetic();
    let dir = std::path::Path::new(DATA);
    if std::env::var_os("BLESS").is_some() {
        write_days(&rows, dir).unwrap();
    }
    let mut on_disk = Vec::new();
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    names.sort();
    for p in names {
        for line in std::fs::read_to_string(p).unwrap().lines() {
            on_disk.push(Row::parse(line).unwrap());
        }
    }
    on_disk.sort_by_key(|r| (r.queued_us, r.run_id.clone()));
    let mut want = rows;
    want.sort_by_key(|r| (r.queued_us, r.run_id.clone()));
    assert_eq!(
        on_disk, want,
        "run `BLESS=1 cargo test -p kbf-estimator-replay` to rewrite it"
    );
}

/// The null arm against the record, on the checked-in synthetic trace: with the estimator
/// off, the replay's own-limit kills equal the record's within 1%. Catches a kill rule
/// that is not the container's limit (1.5 x booking + 512 MiB), a first booking that is
/// not the record's, a ladder that is not the scheduler's, and operations dropped or
/// split by the chaining.
#[test]
fn estimator_off_reproduces_the_recorded_kills() {
    let t = load(std::path::Path::new(DATA), &params()).unwrap();
    assert!(
        t.recorded.kills >= 30,
        "a trace with kills to reproduce: {:?}",
        t.recorded
    );
    assert_eq!(t.funnel.dropped.len(), 0, "{:?}", t.funnel);
    let null = simulate(&t, Arm::Null, &params()).unwrap();
    let f = fidelity(&t, &null);
    assert!(f.within_percent(1), "{f:?}");
    assert_eq!(
        f.simulated, f.recorded,
        "the synthetic record is consistent: exact"
    );
    // The attempts too, not only the kills.
    assert_eq!(null.attempts.len() as u64, t.recorded.attempts);
}

/// Catches a key built without the instance: the same shape in two instances is two keys,
/// so a kill in one raises nothing for the other (here, through the rows).
#[test]
fn the_key_carries_the_instance() {
    let mut a = vec![
        kill("da", 0, 10, 4 * GIB, Some(16 * GIB)),
        attempt("da", 12, 10, "ok", Some(10 * GIB)),
    ];
    a.iter_mut().for_each(|r| r.instance = "a".into());
    let mut b = attempt("db", 4_000, 10, "ok", Some(GIB));
    b.instance = "b".into();
    let mut again = attempt("da2", 5_000, 10, "ok", Some(GIB));
    again.instance = "a".into();
    a.extend([b, again]);
    let t = build(a, &params());
    assert_ne!(t.ops[1].key, t.ops[2].key);
    let floor = simulate(&t, Arm::KeyFloor, &params()).unwrap();
    let booked = |digest: &str| {
        let i = t.ops.iter().position(|o| o.digest == digest).unwrap();
        floor.ops[i].submitted_memory
    };
    assert_eq!(
        booked("db"),
        4 * GIB,
        "instance b is not raised by a's kill"
    );
    assert_eq!(booked("da2"), 16 * GIB, "instance a is");
}

/// Catches a first booking assumed without saying so: a success with no recorded booking
/// is counted in the funnel, a kill with one is not.
#[test]
fn an_assumed_first_booking_is_counted() {
    let rows = vec![
        attempt("a", 0, 5, "ok", Some(GIB)),
        kill("b", 0, 5, 4 * GIB, Some(16 * GIB)),
        attempt("b", 7, 5, "ok", Some(GIB)),
    ];
    let t = build(rows, &params());
    assert_eq!(t.funnel.assumed_first_rung, 1);
}

/// Catches arithmetic that overflows on absurd rows: times and bookings near u64::MAX
/// must load, chain, replay and report without a panic.
#[test]
fn absurd_numbers_do_not_panic() {
    let mut k = kill("a", 0, 5, u64::MAX, Some(u64::MAX));
    k.queued_us = u64::MAX - 10;
    k.started_us = Some(u64::MAX - 9);
    k.ended_us = Some(u64::MAX - 1);
    let mut ok = attempt("a", 0, 5, "ok", Some(u64::MAX));
    ok.queued_us = u64::MAX - 5;
    ok.started_us = Some(u64::MAX - 4);
    ok.ended_us = Some(u64::MAX);
    ok.cgroup.as_mut().unwrap().wall_us = Some(u64::MAX);
    let mut early = attempt("b", 0, 5, "ok", Some(GIB));
    early.queued_us = 0;
    early.started_us = Some(1);
    early.ended_us = Some(2);
    let p = Params {
        train_days: u32::MAX,
        score_days: u32::MAX,
        heartbeat_ms: u64::MAX,
        ..params()
    };
    let t = build(vec![k, ok, early], &p);
    assert_eq!(t.ops.len(), 2);
    let runs: Vec<_> = [Arm::Null, Arm::KeyFloor]
        .iter()
        .filter_map(|a| simulate(&t, *a, &p).ok())
        .collect();
    let text = kbf_estimator_replay::replay::report::render(&t, &p, &runs);
    assert!(text.contains("scored days"));
}
