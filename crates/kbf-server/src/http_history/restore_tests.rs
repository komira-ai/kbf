//! History records back into rows. Each test names the defect it catches.

use kbf_history::records::{
    Cgroup, HitHow, HitRecord, MemoryKill, Output as WireOutput, PhaseTimes, RunOutcome, RunRecord,
    Timestamps, WireDigest,
};
use kbf_invocation::{AttemptEnd, How, Outcome, RunUsage, TargetKind, WallTime};
use kbf_sched::RequeueReason;
use kbf_types::{Digest, DigestFunction, Failure};

use super::rows as to_rows;
use super::{INVALID_ARGUMENT, RESOURCE_EXHAUSTED, UNAVAILABLE};

fn wire(n: u8) -> WireDigest {
    WireDigest::new([n; 32], u64::from(n))
}

fn digest(n: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n))
}

/// A run of action `n` that ended with `outcome`, committed at `ms`, sequence `seq`.
fn run(n: u8, ms: u64, seq: u32, outcome: RunOutcome) -> RunRecord {
    RunRecord {
        time_ms: ms,
        seq,
        instance: "main".to_owned(),
        action: Some(wire(n)),
        node: "node-a".to_owned(),
        lease_epoch: 3,
        lease_seq: u64::from(seq) + 7,
        outcome: outcome.into(),
        cfg: "-".to_owned(),
        timestamps: Some(Timestamps {
            queued_us: 100,
            granted_us: 200,
            ..Timestamps::default()
        }),
        ..RunRecord::default()
    }
}

fn hit(n: u8, ms: u64, row: u32, how: HitHow) -> HitRecord {
    HitRecord {
        time_ms: ms,
        instance: "main".to_owned(),
        action: Some(wire(n)),
        how: how.into(),
        row,
    }
}

/// Catches: rows in the order the reader happened to return the records (runs before
/// hits, whatever their times), a run after a hit of the same millisecond, two runs of
/// one action making two rows (or the second one's `how` replacing the first's), and a
/// hit's answer not being OK.
#[test]
fn rows_come_in_the_order_the_records_were_written_one_per_action() {
    let runs = [
        run(2, 20, 0, RunOutcome::Ok),
        run(3, 30, 1, RunOutcome::Ok),
        run(2, 40, 2, RunOutcome::Ok),
    ];
    let hits = [
        hit(1, 10, 0, HitHow::CacheHitExecute),
        hit(4, 30, 1, HitHow::CacheHit),
        hit(5, 31, 2, HitHow::Unspecified),
        // A hit of an action that ran later keeps the row's first answer.
        hit(3, 5, 3, HitHow::CacheHit),
    ];
    let (rows, skipped) = to_rows(&runs, &hits);
    assert_eq!(skipped, 0);
    let seen: Vec<_> = rows.iter().map(|r| (r.action, r.how)).collect();
    assert_eq!(
        seen,
        [
            (digest(3), How::CacheHit),
            (digest(1), How::CacheHitExecute),
            (digest(2), How::Ran),
            // At 30 ms: the run of 3 (already a row), then the hit of 4.
            (digest(4), How::CacheHit),
            (digest(5), How::CacheHit),
        ]
    );
    let two = &rows[2];
    assert_eq!(
        two.attempts.len(),
        2,
        "both runs of action 2 are its attempts"
    );
    let hit_row = &rows[1];
    assert!(hit_row.attempts.is_empty());
    assert_eq!(hit_row.outcome, Some(Outcome::Ok { exit_code: 0 }));
    // Action 3 was hit, then ran: the row has the hit's answer and the run's attempt.
    assert_eq!(rows[0].attempts.len(), 1);
    assert_eq!(rows[0].outcome, Some(Outcome::Ok { exit_code: 0 }));
}

/// Catches: a record with a digest that is not a SHA-256 (a broken one) made into a row
/// of a made-up action, and not counted for the record to say it is incomplete.
#[test]
fn a_record_with_a_broken_digest_is_skipped_and_counted() {
    let mut broken = run(1, 10, 0, RunOutcome::Ok);
    broken.action = Some(WireDigest {
        hash: vec![1, 2, 3],
        size: 3,
    });
    let mut none = run(2, 11, 1, RunOutcome::Ok);
    none.action = None;
    let mut bad_hit = hit(3, 12, 0, HitHow::CacheHit);
    bad_hit.action = None;
    let (rows, skipped) = to_rows(&[broken, none, run(9, 13, 2, RunOutcome::Ok)], &[bad_hit]);
    assert_eq!(skipped, 3);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].action, digest(9));
}

/// Catches: an attempt that loses what the record keeps: its lease, worker, run id, its
/// own queue time (the first submission given instead, or for a record without it, no
/// time at all), a time that was not reported made the epoch, the nine REAPI times, the
/// cgroup block (the wall time above all), NUMA nodes that were not reported listed as
/// zero bytes, and a booking or cap of 0 made `Some(0)`.
#[test]
fn an_attempt_carries_what_the_record_keeps() {
    let mut r = run(1, 1_000, 4, RunOutcome::Ok);
    r.attempt_queued_us = 150;
    r.started_us = 250;
    r.ended_us = 900;
    r.booked_memory_bytes = 4096;
    r.cap_bytes = 8192;
    r.timestamps = Some(Timestamps {
        queued_us: 100,
        granted_us: 200,
        worker_start_us: 300,
        input_fetch_start_us: 310,
        input_fetch_completed_us: 320,
        execution_start_us: 330,
        execution_completed_us: 340,
        output_upload_start_us: 350,
        output_upload_completed_us: 360,
        worker_completed_us: 370,
    });
    r.cgroup = Some(Cgroup {
        cpu_usage_us: 11,
        cpu_user_us: 12,
        cpu_system_us: 13,
        periods: 14,
        throttled_periods: 15,
        throttled_us: 16,
        memory_peak_bytes: Some(17),
        numa_bytes: vec![5, 0, 7],
        wall_us: Some(18),
        ..Cgroup::default()
    });
    let (rows, _) = to_rows(&[r.clone()], &[]);
    let a = &rows[0].attempts[0];
    assert_eq!((a.lease.term, a.lease.seq), (3, 11));
    assert_eq!(a.worker.as_str(), "node-a");
    assert_eq!(a.run_id, Some(r.run_id().to_bytes()));
    let us = |t: Option<WallTime>| t.map(WallTime::unix_micros);
    assert_eq!(a.queued.unix_micros(), 150);
    assert_eq!(a.granted.unix_micros(), 200);
    assert_eq!((us(a.started), us(a.ended)), (Some(250), Some(900)));
    let measured = a.run.as_deref().expect("a run");
    let times: Vec<_> = measured.times.named().iter().map(|(_, t)| us(*t)).collect();
    let want: Vec<_> = [100, 300, 310, 320, 330, 340, 350, 360, 370]
        .into_iter()
        .map(Some)
        .collect();
    assert_eq!(times, want);
    assert_eq!(
        (measured.booked_memory_bytes, measured.cap_bytes),
        (Some(4096), Some(8192))
    );
    let u: &RunUsage = measured.usage.as_ref().expect("usage");
    assert_eq!(u.wall_micros, 18);
    assert_eq!(
        [
            u.cpu_usage_micros,
            u.cpu_user_micros,
            u.cpu_system_micros,
            u.cpu_periods,
            u.cpu_throttled_periods,
            u.cpu_throttled_micros,
            u.memory_peak_bytes
        ],
        [11, 12, 13, 14, 15, 16, 17].map(Some)
    );
    let numa: Vec<_> = u.numa.iter().map(|n| (n.node, n.bytes)).collect();
    assert_eq!(numa, [(0, 5), (2, 7)], "an unreported node is left out");

    // An older record: no attempt queue time, no start, no end, nothing booked.
    let mut older = run(1, 1_000, 0, RunOutcome::Ok);
    older.timestamps = Some(Timestamps {
        queued_us: 100,
        granted_us: 200,
        worker_start_us: 300,
        ..Timestamps::default()
    });
    let (rows, _) = to_rows(&[older], &[]);
    let a = &rows[0].attempts[0];
    assert_eq!(
        a.queued.unix_micros(),
        100,
        "the first submission stands in"
    );
    assert_eq!((a.started, a.ended), (None, None));
    let measured = a.run.as_deref().expect("a run");
    assert_eq!(
        (measured.booked_memory_bytes, measured.cap_bytes),
        (None, None)
    );
    assert!(measured.usage.is_none());
}

/// Catches: a lease the scheduler took no report for (nothing but its queue and grant
/// times in the record) given a run block of made-up times, which the page would show as
/// `reapi_times` and the summary count in its phase totals.
#[test]
fn a_lease_nothing_was_measured_for_has_no_run() {
    let (rows, _) = to_rows(&[run(1, 10, 0, RunOutcome::Lost)], &[]);
    assert!(rows[0].attempts[0].run.is_none());
    let mut no_times = run(1, 10, 0, RunOutcome::Lost);
    no_times.timestamps = None;
    let (rows, _) = to_rows(&[no_times], &[]);
    let a = &rows[0].attempts[0];
    assert!(a.run.is_none());
    assert_eq!(a.queued.unix_micros(), 0);
}

fn killed(kill: MemoryKill) -> RunRecord {
    let mut r = run(1, 10, 0, RunOutcome::MemoryKilled);
    r.memory_kill = kill.into();
    r.booked_memory_bytes = 1_000;
    r.raised_memory_bytes = 2_000;
    r.reason = "why".to_owned();
    r
}

/// Catches: an attempt's end or the row's outcome read wrong from the run's class: an
/// exit code of 0 and non-zero swapped, a timeout not a timeout, a memory kill of one
/// kind taken for the other (the page would say `out_of_memory` for a node's kill), a
/// failure the daemon reported lost as one the client caused, a class with no end
/// given one, an attempt another followed not a requeue, and the requeue of an own-limit
/// kill losing its booking and the raised booking.
#[test]
fn the_end_and_outcome_follow_the_class() {
    let reason = || "why".to_owned();
    let failed = |code| {
        Some(Outcome::Failed {
            code,
            message: reason(),
        })
    };
    let mut nonzero = run(1, 10, 0, RunOutcome::Failed);
    nonzero.exit_code = 3;
    let mut timed = run(1, 10, 0, RunOutcome::TimedOut);
    timed.reason = reason();
    let mut invalid = run(1, 10, 0, RunOutcome::Failed);
    invalid.reason = reason();
    let mut infra = run(1, 10, 0, RunOutcome::Infra);
    infra.reason = reason();
    let mut lost = run(1, 10, 0, RunOutcome::Lost);
    lost.reason = reason();
    let accepted = Some(AttemptEnd::Accepted);
    let cases = [
        (
            "ok",
            run(1, 10, 0, RunOutcome::Ok),
            accepted.clone(),
            Some(Outcome::Ok { exit_code: 0 }),
        ),
        (
            "non-zero exit",
            nonzero,
            accepted,
            Some(Outcome::Ok { exit_code: 3 }),
        ),
        (
            "failed, exit 0",
            invalid,
            Some(AttemptEnd::Failed(Failure::Invalid, reason())),
            failed(INVALID_ARGUMENT),
        ),
        (
            "timed out",
            timed,
            Some(AttemptEnd::Failed(Failure::Timeout, reason())),
            Some(Outcome::TimedOut),
        ),
        (
            "own limit",
            killed(MemoryKill::OwnLimit),
            Some(AttemptEnd::Failed(Failure::OutOfMemory, reason())),
            failed(RESOURCE_EXHAUSTED),
        ),
        (
            "node pressure",
            killed(MemoryKill::NodePressure),
            Some(AttemptEnd::Failed(Failure::NodeMemoryPressure, reason())),
            failed(UNAVAILABLE),
        ),
        (
            "memory killed, no kind",
            killed(MemoryKill::None),
            Some(AttemptEnd::Failed(Failure::NodeMemoryPressure, reason())),
            failed(UNAVAILABLE),
        ),
        (
            "infra",
            infra,
            Some(AttemptEnd::Failed(Failure::Infra, reason())),
            failed(UNAVAILABLE),
        ),
        (
            "lost",
            lost,
            Some(AttemptEnd::Failed(
                Failure::Lost(kbf_types::LostWhy::Fenced),
                reason(),
            )),
            failed(UNAVAILABLE),
        ),
        (
            "unspecified",
            run(1, 10, 0, RunOutcome::Unspecified),
            None,
            None,
        ),
        (
            "cancelled",
            run(1, 10, 0, RunOutcome::Cancelled),
            None,
            None,
        ),
    ];
    for (what, record, end, outcome) in cases {
        let (rows, _) = to_rows(&[record], &[]);
        assert_eq!(rows[0].attempts[0].end, end, "{what}");
        assert_eq!(rows[0].outcome, outcome, "{what}");
    }
}

/// Catches: an attempt that another followed given a final end (the row would show an
/// action that failed twice as two failures), a requeue of the wrong kind, and a class
/// with no kind of requeue (a memory kill of no kind, a timeout) not given the
/// general one.
#[test]
fn an_attempt_another_followed_was_given_up() {
    let then = run(1, 99, 9, RunOutcome::Ok);
    let cases = [
        (
            killed(MemoryKill::OwnLimit),
            Some(AttemptEnd::Requeued(RequeueReason::OutOfMemory {
                booked: 1_000,
                raised: 2_000,
            })),
        ),
        (
            killed(MemoryKill::NodePressure),
            Some(AttemptEnd::Requeued(RequeueReason::NodeMemoryPressure)),
        ),
        (
            killed(MemoryKill::None),
            Some(AttemptEnd::Requeued(RequeueReason::Silent)),
        ),
        (
            run(1, 10, 0, RunOutcome::Infra),
            Some(AttemptEnd::Requeued(RequeueReason::Infra)),
        ),
        (
            run(1, 10, 0, RunOutcome::Lost),
            Some(AttemptEnd::Requeued(RequeueReason::Silent)),
        ),
        (
            run(1, 10, 0, RunOutcome::TimedOut),
            Some(AttemptEnd::Requeued(RequeueReason::Silent)),
        ),
        (run(1, 10, 0, RunOutcome::Unspecified), None),
        (run(1, 10, 0, RunOutcome::Cancelled), None),
    ];
    for (first, end) in cases {
        let (rows, _) = to_rows(&[first.clone(), then.clone()], &[]);
        assert_eq!(rows[0].attempts[0].end, end, "{first:?}");
        assert_eq!(rows[0].attempts[1].end, Some(AttemptEnd::Accepted));
        assert_eq!(rows[0].outcome, Some(Outcome::Ok { exit_code: 0 }));
    }
}

/// Catches: a shape made for an action that has none (every row would carry one), a
/// configuration of `-` shown as a hash, a target kind that does not round-trip, a
/// target with no kind made one, and a test flag lost.
#[test]
fn a_shape_is_made_only_when_the_record_names_one() {
    let (rows, _) = to_rows(&[run(1, 10, 0, RunOutcome::Ok)], &[]);
    assert!(rows[0].shape.is_none());
    let mut r = run(1, 10, 0, RunOutcome::Ok);
    r.shape = 0x0123_4567_89ab_cdef;
    r.cfg = "abc123".to_owned();
    r.test = true;
    for (kind, want) in [
        (1, Some(TargetKind::Target)),
        (2, Some(TargetKind::Anon)),
        (3, Some(TargetKind::Bxl)),
        (0, None),
    ] {
        r.target = "root//pkg:name".to_owned();
        r.target_kind = kind;
        let (rows, _) = to_rows(&[r.clone()], &[]);
        let shape = rows[0].shape.as_deref().expect("a shape");
        assert_eq!(shape.key_text(), "0123456789abcdef");
        assert_eq!(shape.cfg.as_deref(), Some("abc123"));
        assert!(shape.test);
        assert_eq!(shape.target.as_ref().map(|t| t.kind), want);
        if want.is_some() {
            assert_eq!(
                shape.target.as_ref().map(|t| t.label.value.as_str()),
                Some("root//pkg:name")
            );
        }
    }
    // Only a test flag, or only a target, is a shape too; only `-` is none.
    let mut only_test = run(1, 10, 0, RunOutcome::Ok);
    only_test.test = true;
    let mut only_target = run(1, 10, 0, RunOutcome::Ok);
    only_target.target = "t".to_owned();
    only_target.target_kind = 1;
    for r in [only_test, only_target] {
        let (rows, _) = to_rows(&[r], &[]);
        assert!(rows[0].shape.is_some());
    }
}

/// Catches: the result, logs and outputs of the run that ended the action not copied (a
/// page with no stdout to link), those of an earlier attempt copied instead, an output
/// with a broken digest listed, a directory taken for a file, an exit code for a run
/// that timed out, and a listing made for a run that kept none.
#[test]
fn the_last_runs_result_logs_and_outputs_are_the_rows_listing() {
    let mut first = run(1, 10, 0, RunOutcome::Infra);
    first.stdout = Some(wire(50));
    let mut last = run(1, 20, 1, RunOutcome::Failed);
    last.exit_code = 2;
    last.command = Some(wire(60));
    last.result = Some(wire(61));
    last.stderr = Some(wire(62));
    last.outputs = vec![
        WireOutput {
            path: "out/a".to_owned(),
            digest: Some(wire(70)),
            is_dir: false,
        },
        WireOutput {
            path: "out/dir".to_owned(),
            digest: Some(wire(71)),
            is_dir: true,
        },
        WireOutput {
            path: "out/broken".to_owned(),
            digest: None,
            is_dir: false,
        },
    ];
    let (rows, _) = to_rows(&[first, last], &[]);
    let l = rows[0].listing.as_ref().expect("a listing");
    assert_eq!(l.result, Some(digest(61)));
    assert_eq!((l.stdout, l.stderr), (None, Some(digest(62))));
    assert_eq!(l.exit_code, Some(2));
    let outs: Vec<_> = l
        .outputs
        .iter()
        .map(|o| (o.path.as_str(), o.directory))
        .collect();
    assert_eq!(outs, [("out/a", false), ("out/dir", true)]);
    assert_eq!(rows[0].command, Some(digest(60)));

    let mut timed = run(2, 30, 2, RunOutcome::TimedOut);
    timed.stdout = Some(wire(80));
    let (rows, _) = to_rows(&[timed], &[]);
    assert_eq!(rows[0].listing.as_ref().and_then(|l| l.exit_code), None);
    let (rows, _) = to_rows(&[run(3, 40, 3, RunOutcome::Ok)], &[]);
    assert!(rows[0].listing.is_none());
}

/// Catches: a run block made, or not made, on one evidence alone: a record with only a
/// cgroup block, only a booking, only a cap, or only a time other than its queue time
/// has measured something; one with none has not.
#[test]
fn any_one_measurement_makes_a_run_block() {
    let bare = || run(1, 10, 0, RunOutcome::Lost);
    let mut usage = bare();
    usage.cgroup = Some(Cgroup::default());
    let mut booked = bare();
    booked.booked_memory_bytes = 1;
    let mut capped = bare();
    capped.cap_bytes = 1;
    let mut timed = bare();
    timed.timestamps = Some(Timestamps {
        worker_completed_us: 5,
        ..Timestamps::default()
    });
    for (what, r) in [
        ("a cgroup block", usage),
        ("a booking", booked),
        ("a cap", capped),
        ("a time", timed),
    ] {
        let (rows, _) = to_rows(&[r], &[]);
        assert!(rows[0].attempts[0].run.is_some(), "{what}");
    }
    let (rows, _) = to_rows(&[bare()], &[]);
    assert!(rows[0].attempts[0].run.is_none());
}

/// Catches: a shape made on one evidence alone wrongly refused (only a key, only a
/// configuration, only a test flag, only a target), one made of a configuration of `-`
/// or of none (a default record), and a target with no label or no kind made.
#[test]
fn any_one_name_makes_a_shape_and_no_name_makes_none() {
    let some = |f: &dyn Fn(&mut RunRecord)| {
        let mut r = run(1, 10, 0, RunOutcome::Ok);
        r.cfg = String::new();
        f(&mut r);
        let (rows, _) = to_rows(&[r], &[]);
        rows[0].shape.clone()
    };
    assert!(some(&|_| {}).is_none(), "nothing named");
    assert!(some(&|r| r.shape = 5).is_some(), "a key");
    assert!(
        some(&|r| r.cfg = "abc".to_owned()).is_some(),
        "a configuration"
    );
    assert!(some(&|r| r.test = true).is_some(), "a test flag");
    assert!(some(&|r| r.cfg = "-".to_owned()).is_none(), "`-` is none");
    // A kind with no label, a label with no kind: no target, so no shape.
    assert!(some(&|r| r.target_kind = 1).is_none());
    assert!(some(&|r| r.target = "t".to_owned()).is_none());
    let only_key = some(&|r| r.shape = 5).expect("a shape");
    assert_eq!(only_key.cfg, None, "a configuration of none is none");
}

/// Catches: a listing missing for a run that kept only one of what it lists (a stderr
/// alone, outputs alone, a result alone), and an exit code made up for a run that did
/// not exit, or missing for one that did.
#[test]
fn any_one_result_makes_a_listing_and_an_exit_code_is_the_runs() {
    let listed = |f: &dyn Fn(&mut RunRecord), outcome| {
        let mut r = run(1, 10, 0, outcome);
        f(&mut r);
        let (rows, _) = to_rows(&[r], &[]);
        rows[0].listing.clone()
    };
    let none = listed(&|_| {}, RunOutcome::Ok);
    assert!(none.is_none());
    let stderr = listed(&|r| r.stderr = Some(wire(1)), RunOutcome::Ok).expect("a listing");
    assert_eq!(stderr.exit_code, Some(0), "an ok run exited 0");
    let result = listed(&|r| r.result = Some(wire(2)), RunOutcome::Failed).expect("a listing");
    assert_eq!(
        result.exit_code,
        Some(0),
        "a failed run's exit code is kept"
    );
    let outputs = listed(
        &|r| {
            r.outputs = vec![WireOutput {
                path: "p".to_owned(),
                digest: Some(wire(3)),
                is_dir: false,
            }];
        },
        RunOutcome::Infra,
    )
    .expect("a listing");
    assert_eq!(outputs.exit_code, None, "a run the farm failed has none");
    // Outputs that all have broken digests list nothing.
    let broken = listed(
        &|r| {
            r.outputs = vec![WireOutput {
                path: "p".to_owned(),
                digest: None,
                is_dir: false,
            }];
        },
        RunOutcome::Ok,
    );
    assert!(broken.is_none());
}

/// A one-attempt row whose attempt measured `usage`.
fn measured_row(usage: RunUsage) -> kbf_invocation::ActionRow {
    use kbf_invocation::{ActionRow, Attempt, ReapiTimes, Run};
    use kbf_types::{LeaseId, WorkerId};
    let at = WallTime::from_unix_micros;
    ActionRow {
        action: digest(1),
        instance: "main".to_owned(),
        target: None,
        mnemonic: None,
        configuration: None,
        action_id: None,
        how: How::Ran,
        operation: Some("operations/x".to_owned()),
        attempts: vec![Attempt {
            lease: LeaseId::new(3, 9),
            worker: WorkerId::new("node-a"),
            queued: at(10_000),
            granted: at(20_000),
            started: Some(at(30_000)),
            ended: Some(at(40_000)),
            end: Some(AttemptEnd::Accepted),
            run: Some(Box::new(Run {
                times: ReapiTimes::default(),
                usage: Some(usage),
                booked_memory_bytes: None,
                booked_millicpus: None,
                cap_bytes: None,
                phase_times: Default::default(),
            })),
            run_id: None,
        }],
        outcome: Some(Outcome::Ok { exit_code: 0 }),
        listing: None,
        outputs_truncated: false,
        command: None,
        shape: None,
        evidence: None,
        operation_id: None,
    }
}

/// What a usage becomes when the tap writes the run record, the record is encoded and
/// decoded as the history chunk would hold it, and the read side restores it.
fn through_history(usage: RunUsage) -> RunUsage {
    use prost::Message as _;
    let row = measured_row(usage);
    let record = crate::history::tap::run_record(&row, &row.attempts[0]);
    let record = RunRecord::decode(record.encode_to_vec().as_slice()).expect("decodes");
    let (rows, _) = to_rows(&[record], &[]);
    let run = rows[0].attempts[0].run.as_deref().expect("a run");
    run.usage.clone().expect("usage")
}

/// Catches: the basis dropped on the write or the read side (a Mac run restored as a
/// Linux one, so its footprint is pooled with cgroup peaks), the footprint block lost,
/// a cgroup block written beside a footprint one, a footprint field that was absent
/// restored as 0, and the CPU pressure or the booking lost (or made 0 when unreported).
#[test]
fn the_basis_the_footprint_and_the_cpu_evidence_survive_the_history_round_trip() {
    use kbf_invocation::{FootprintUsage, UsageBasis};
    let mac = RunUsage {
        wall_micros: 24,
        basis: UsageBasis::MacFootprint,
        footprint: Some(FootprintUsage {
            peak_footprint_bytes: Some(21),
            cpu_user_micros: Some(22),
            cpu_system_micros: None,
            wall_micros: Some(24),
        }),
        node_cpu_pressure_centi: Some(0),
        booked_millicpus: Some(2000),
        ..RunUsage::default()
    };
    assert_eq!(through_history(mac.clone()), mac);

    let linux = RunUsage {
        wall_micros: 18,
        basis: UsageBasis::Cgroup,
        cpu_usage_micros: Some(11),
        cpu_user_micros: Some(12),
        cpu_system_micros: Some(13),
        cpu_periods: Some(0),
        cpu_throttled_periods: Some(0),
        cpu_throttled_micros: Some(0),
        memory_peak_bytes: Some(17),
        node_cpu_pressure_centi: Some(1234),
        booked_millicpus: Some(1500),
        memory_events_max: Some(2),
        memory_events_oom_kill: Some(0),
        memory_swap_peak_bytes: Some(3),
        memory_anon_swap_peak_bytes: Some(4),
        cpu_pressure_some_micros: Some(5),
        memory_pressure_some_micros: Some(6),
        memory_pressure_full_micros: Some(7),
        cpus_effective: Some(16),
        ..RunUsage::default()
    };
    assert_eq!(through_history(linux.clone()), linux);

    // A kernel without `memory.peak`: no peak comes back, not a zero one.
    let old = RunUsage {
        basis: UsageBasis::Cgroup,
        cpu_usage_micros: Some(1),
        cpu_user_micros: Some(1),
        cpu_system_micros: Some(0),
        cpu_periods: Some(0),
        cpu_throttled_periods: Some(0),
        cpu_throttled_micros: Some(0),
        ..RunUsage::default()
    };
    assert_eq!(through_history(old).memory_peak_bytes, None);

    // A daemon that reports neither: they come back absent, not zero.
    let bare = RunUsage {
        basis: UsageBasis::Cgroup,
        ..RunUsage::default()
    };
    let back = through_history(bare);
    assert_eq!(
        (back.node_cpu_pressure_centi, back.booked_millicpus),
        (None, None)
    );
    assert_eq!(back.footprint, None);
    assert_eq!(back.basis, UsageBasis::Cgroup);
}

/// Catches: the booking of an accepted run lost on the history round trip (the attempt
/// read back from history without what its lease held), either axis zeroed or swapped.
#[test]
fn the_booking_of_an_accepted_run_survives_the_history_round_trip() {
    use prost::Message as _;
    let mut row = measured_row(RunUsage::default());
    row.attempts[0].run.as_mut().expect("run").booked_millicpus = Some(2_500);
    row.attempts[0]
        .run
        .as_mut()
        .expect("run")
        .booked_memory_bytes = Some(3 << 30);
    let record = crate::history::tap::run_record(&row, &row.attempts[0]);
    let record = RunRecord::decode(record.encode_to_vec().as_slice()).expect("decodes");
    let (rows, _) = to_rows(&[record], &[]);
    let run = rows[0].attempts[0].run.as_deref().expect("a run");
    assert_eq!(
        (run.booked_millicpus, run.booked_memory_bytes),
        (Some(2_500), Some(3 << 30))
    );
}

/// Catches: a footprint basis record restored through its (absent) cgroup block as a
/// block of zeros, and a record written before the basis existed losing its cgroup
/// block.
#[test]
fn a_footprint_record_has_no_cgroup_fields_and_an_older_record_reads_its_cgroup_block() {
    use kbf_history::records::{Footprint, UsageBasis};
    let mut mac = run(1, 1_000, 0, RunOutcome::Ok);
    mac.basis = UsageBasis::MacFootprint.into();
    mac.footprint = Some(Footprint {
        peak_footprint_bytes: Some(5),
        ..Footprint::default()
    });
    let (rows, _) = to_rows(&[mac], &[]);
    let u = rows[0].attempts[0]
        .run
        .as_deref()
        .unwrap()
        .usage
        .clone()
        .unwrap();
    assert_eq!(u.memory_peak_bytes, None);
    assert_eq!(u.cpu_usage_micros, None);
    assert_eq!(u.footprint.unwrap().peak_footprint_bytes, Some(5));

    // Written before the basis: a cgroup block and nothing else.
    let mut older = run(2, 2_000, 0, RunOutcome::Ok);
    older.cgroup = Some(Cgroup {
        memory_peak_bytes: Some(7),
        ..Cgroup::default()
    });
    let (rows, _) = to_rows(&[older], &[]);
    let u = rows[0].attempts[0]
        .run
        .as_deref()
        .unwrap()
        .usage
        .clone()
        .unwrap();
    assert_eq!(u.memory_peak_bytes, Some(7));
    assert_eq!(u.basis, kbf_invocation::UsageBasis::Unspecified);
    assert_eq!(u.footprint, None);
}

/// Catches: the operation id or the submit evidence dropped when a record becomes a row
/// again (the history view would lose what the live view had), an absent walk total
/// restored as 0, the two sources swapped, and a record written before the fields
/// (nothing set) coming back with evidence claiming the client named nothing.
#[test]
fn the_operation_and_the_submit_evidence_come_back_and_old_records_have_none() {
    use kbf_history::records::BookSource;
    use kbf_types::OperationId;

    let mut told = run(1, 1_000, 0, RunOutcome::Ok);
    told.operation_id = Some(77);
    told.cpu_source = BookSource::Hint.into();
    told.memory_source = BookSource::Default.into();
    told.input_bytes = Some(5_000);
    let (rows, _) = to_rows(&[told], &[]);
    assert_eq!(rows[0].operation_id, Some(OperationId(77)));
    let e = rows[0].evidence.expect("evidence");
    assert_eq!(e.cpu_source, kbf_invocation::BookSource::Hint);
    assert_eq!(e.memory_source, kbf_invocation::BookSource::Default);
    assert_eq!((e.input_bytes, e.input_files), (Some(5_000), None));

    let (rows, _) = to_rows(&[run(2, 1_000, 0, RunOutcome::Ok)], &[]);
    assert_eq!(rows[0].operation_id, None);
    assert_eq!(rows[0].evidence, None);
}

/// Catches: submit evidence dropped when a record kept only one of its fields: each of the
/// memory source, the input byte count and the input file count alone must come back as
/// evidence (the other fields unnamed), not be read as a record that kept none.
#[test]
fn a_record_that_kept_one_evidence_field_comes_back_with_evidence() {
    use kbf_history::records::BookSource;

    let mut memory_only = run(1, 1_000, 0, RunOutcome::Ok);
    memory_only.memory_source = BookSource::Hint.into();
    let mut bytes_only = run(2, 1_000, 0, RunOutcome::Ok);
    bytes_only.input_bytes = Some(9);
    let mut files_only = run(3, 1_000, 0, RunOutcome::Ok);
    files_only.input_files = Some(4);
    let (rows, _) = to_rows(&[memory_only, bytes_only, files_only], &[]);

    let memory = rows[0].evidence.expect("memory source alone");
    assert_eq!(memory.memory_source, kbf_invocation::BookSource::Hint);
    assert_eq!((memory.input_bytes, memory.input_files), (None, None));
    let bytes = rows[1].evidence.expect("input bytes alone");
    assert_eq!((bytes.input_bytes, bytes.input_files), (Some(9), None));
    let files = rows[2].evidence.expect("input files alone");
    assert_eq!((files.input_bytes, files.input_files), (None, Some(4)));
}

/// Catches the marker lost on a restore: a record of a task's claim restores to a run that
/// says so, a record of a daemon's run to a measured one, and a record written before the
/// marker (unspecified) to a measured one, which is what such a record was.
#[test]
fn an_attempt_keeps_where_its_phase_times_came_from() {
    let read = |marker: PhaseTimes| {
        let mut r = run(1, 1_000, 4, RunOutcome::Ok);
        r.phase_times = marker.into();
        r.timestamps = Some(Timestamps {
            queued_us: 100,
            granted_us: 200,
            worker_start_us: 300,
            ..Timestamps::default()
        });
        let (rows, _) = to_rows(&[r], &[]);
        rows[0].attempts[0]
            .run
            .as_deref()
            .expect("a run")
            .phase_times
    };
    assert_eq!(
        read(PhaseTimes::TaskClaim),
        kbf_invocation::PhaseTimes::TaskClaim
    );
    assert_eq!(
        read(PhaseTimes::Measured),
        kbf_invocation::PhaseTimes::Measured
    );
    assert_eq!(
        read(PhaseTimes::Unspecified),
        kbf_invocation::PhaseTimes::Measured
    );
}
