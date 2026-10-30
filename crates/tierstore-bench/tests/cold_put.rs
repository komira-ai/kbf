//! The cold-put cell against a loopback S3 service: what a level writes, what it reports, and
//! when it stops. No test asserts a duration; caps are driven by a count, a byte budget, a zero
//! time budget or a scripted load source.

mod common;

use std::sync::Arc;

use common::{Refusing, Script, TouchOn, Unreadable, calm, proc_tree, rig};
use kbf_testkit::dir::TestDir;
use tierstore_bench::cells::{self, CellId, cold_put};
use tierstore_bench::error::BenchError;
use tierstore_bench::gate::LoadSample;
use tierstore_bench::requestlog::RequestLog;
use tierstore_bench::result::{CellResult, Window};
use tierstore_bench::s3::unix_ms;

fn busy_io() -> LoadSample {
    LoadSample {
        psi_io_avg10: 90.0,
        ..calm()
    }
}

fn busy_load() -> LoadSample {
    LoadSample {
        loadavg_1m: 99.0,
        ..calm()
    }
}

/// Catches a level that writes the wrong number of objects, under the wrong names, with the
/// wrong size, or that reports fewer latencies than requests: it writes N objects named
/// `<prefix>/<run>/c<level>/<index>`, every request is one CSV row with a latency, one log
/// record, and the result file is the result.
#[tokio::test]
async fn a_level_writes_n_objects_under_the_run_prefix_and_reports_n_latencies() {
    let r = rig(
        "tierstore-bench-cp-level",
        &["--in-flight", "4", "--object-bytes", "4096", "--puts", "12"],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted, None);
    assert_eq!(run.results.len(), 1);
    let result = &run.results[0];
    assert_eq!(result.cell, CellId::ColdPut);
    assert_eq!(result.arm, "direct-c4");
    let pass = &result.passes[0];
    assert_eq!(pass.pass, 0);
    assert!(!pass.discarded);
    assert_eq!(pass.requests, 12);
    assert_eq!(pass.failures, 0);
    assert_eq!(pass.bytes, 12 * 4096);
    assert!(pass.percentiles.is_some());

    let expected: Vec<String> = (0..12).map(|i| format!("bench/c1/r1/c4/{i:08}")).collect();
    assert_eq!(r.fake.keys(), expected);

    let rows = r.csv_rows();
    assert_eq!(rows.len(), 12);
    for row in &rows {
        // run_id,git_sha,binary_digest,cell,arm,pass,window,role,op,bytes,in_flight,
        // started_unix_ms,latency_us,status
        assert_eq!(row[0], "r1");
        assert_eq!(row[3], "cold-put");
        assert_eq!(row[4], "direct-c4");
        assert_eq!(row[7], "cold");
        assert_eq!(row[8], "put");
        assert_eq!(row[9], "4096");
        let in_flight: u32 = row[10].parse().unwrap();
        assert!((1..=4).contains(&in_flight), "{in_flight}");
        row[12].parse::<u64>().unwrap();
        assert_eq!(row[13], "200");
    }

    let records = r.log_records();
    assert_eq!(records.len(), 12);
    assert!(
        records
            .iter()
            .all(|x| x.method == "PUT" && x.status == Some(200))
    );

    let text = std::fs::read_to_string(r.out.join("cold-put-direct-c4.json")).unwrap();
    assert_eq!(&CellResult::from_json(&text).unwrap(), result);
    assert!(
        result.notes.iter().any(|n| n.contains("200=12")),
        "{result:?}"
    );
}

/// Catches levels that share a name space (a second level overwriting or colliding with the
/// first) and an arm label that does not name its level: one result per level, in the order
/// given, each under its own sub-prefix.
#[tokio::test]
async fn each_level_is_its_own_arm_with_its_own_keys() {
    let r = rig(
        "tierstore-bench-cp-levels",
        &["--in-flight", "2,3", "--puts", "5"],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    let arms: Vec<&str> = run.results.iter().map(|x| x.arm.as_str()).collect();
    assert_eq!(arms, ["direct-c2", "direct-c3"]);
    let keys = r.fake.keys();
    assert_eq!(keys.len(), 10);
    assert_eq!(keys.iter().filter(|k| k.contains("/c2/")).count(), 5);
    assert_eq!(keys.iter().filter(|k| k.contains("/c3/")).count(), 5);
    assert_eq!(r.csv_rows().len(), 10);
    // Object size defaults to 1 MiB: the byte count says so.
    assert_eq!(run.results[0].passes[0].bytes, 5 << 20);
}

/// Catches a level that overruns its byte budget: with a budget of five objects, a plan of a
/// hundred writes exactly five and says why it ended.
#[tokio::test]
async fn a_level_stops_at_its_byte_cap_and_says_so() {
    let mut r = rig(
        "tierstore-bench-cp-bytes",
        &[
            "--in-flight",
            "4",
            "--object-bytes",
            "1000",
            "--puts",
            "100",
        ],
    )
    .await;
    r.ctx.config.bytes.pass = 5000;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted, None);
    assert_eq!(r.fake.keys().len(), 5);
    assert_eq!(run.results[0].passes[0].requests, 5);
    assert_eq!(run.results[0].passes[0].bytes, 5000);
    assert!(
        run.results[0].notes.iter().any(|n| n.contains("byte-cap")),
        "{:?}",
        run.results[0].notes
    );
}

/// Catches a time cap that is not honoured: a level with no time left makes no request, says
/// the time cap ended it, and is not a halt of the run (the next level still gets its turn).
#[tokio::test]
async fn a_level_with_no_time_left_writes_nothing_and_says_why() {
    let r = rig(
        "tierstore-bench-cp-time",
        &["--in-flight", "2,4", "--puts", "50", "--level-secs", "0"],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted, None);
    assert_eq!(run.results.len(), 2);
    for result in &run.results {
        assert_eq!(result.passes[0].requests, 0);
        assert!(result.passes[0].percentiles.is_none());
        assert!(
            result.notes.iter().any(|n| n.contains("level-time-cap")),
            "{:?}",
            result.notes
        );
    }
    assert!(r.fake.keys().is_empty());
}

/// Catches a stop file that is ignored or that lets later levels start: with the file present
/// nothing is written, the run reports it halted by the stop file, and the second level is not
/// run.
#[tokio::test]
async fn a_stop_file_halts_the_run_before_any_write() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cp-stop-a");
    let stop = dir.path().join("stop");
    std::fs::write(&stop, "").unwrap();
    let r2 = rig(
        "tierstore-bench-cp-stop-b",
        &[
            "--in-flight",
            "2,4",
            "--puts",
            "10",
            "--stop-file",
            stop.to_str().unwrap(),
        ],
    )
    .await;
    let run = cold_put::run_levels(&r2.ctx, &r2.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted.as_deref(), Some("stop-file"));
    assert_eq!(run.results.len(), 1);
    assert_eq!(run.results[0].passes[0].requests, 0);
    assert!(r2.fake.keys().is_empty());
}

/// Catches a gate that is read but not obeyed before the first level: a busy machine at the
/// start means no request, the pass records the measure that tripped, and the run is halted.
#[tokio::test]
async fn a_busy_machine_at_the_start_writes_nothing_and_names_the_measure() {
    let r = rig("tierstore-bench-cp-gate-start", &["--in-flight", "4,8"]).await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::new([busy_load()]))
        .await
        .unwrap();
    assert!(
        run.halted
            .as_deref()
            .is_some_and(|h| h.contains("loadavg_1m")),
        "{:?}",
        run.halted
    );
    assert_eq!(run.results.len(), 1);
    let pass = &run.results[0].passes[0];
    assert_eq!(pass.requests, 0);
    assert_eq!(pass.gate_tripped.as_deref(), Some("loadavg_1m"));
    assert!(r.fake.keys().is_empty());
}

/// Catches a gate that is only read at the start: the machine turns busy after the first batch,
/// the level ends early with the measure named, its samples are `busy` although the run
/// declared a quiet window, and no further level starts.
#[tokio::test]
async fn a_machine_that_turns_busy_between_batches_ends_the_level_and_the_run() {
    let r = rig(
        "tierstore-bench-cp-gate-mid",
        &[
            "--in-flight",
            "4,8",
            "--puts",
            "400",
            "--gate-every",
            "10",
            "--object-bytes",
            "512",
            "--window",
            "quiet",
        ],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::new([calm(), busy_io()]))
        .await
        .unwrap();
    assert!(run.halted.is_some());
    assert_eq!(run.results.len(), 1);
    let pass = &run.results[0].passes[0];
    assert_eq!(pass.gate_tripped.as_deref(), Some("psi_io_avg10"));
    assert_eq!(pass.window, Window::Busy);
    assert!(pass.requests < 400, "{}", pass.requests);
    assert!(pass.requests >= 10, "{}", pass.requests);
    assert_eq!(u64::try_from(r.fake.keys().len()).unwrap(), pass.requests);
    let rows = r.csv_rows();
    assert!(rows.iter().all(|row| row[6] == "busy"));
}

/// Catches a 412 treated as a failure of the run: a key that already exists answers 412, the
/// level goes on, the 412 is counted as a 412 (in the status counts and in the CSV), it is not
/// a failure, and the other writes are unaffected.
#[tokio::test]
async fn a_412_is_counted_as_a_412_and_is_not_an_error() {
    let r = rig(
        "tierstore-bench-cp-412",
        &["--in-flight", "4", "--puts", "8", "--object-bytes", "256"],
    )
    .await;
    r.put_elsewhere("bench/c1/r1/c4/00000003").await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted, None);
    let pass = &run.results[0].passes[0];
    assert_eq!(pass.requests, 8);
    assert_eq!(pass.failures, 0);
    assert_eq!(pass.bytes, 7 * 256);
    let notes = run.results[0].notes.join("\n");
    assert!(notes.contains("200=7"), "{notes}");
    assert!(notes.contains("412=1"), "{notes}");
    let rows = r.csv_rows();
    assert_eq!(rows.iter().filter(|row| row[13] == "200").count(), 7);
    assert_eq!(rows.iter().filter(|row| row[13] == "412").count(), 1);
    assert_eq!(r.fake.keys().len(), 8);
}

/// Catches a store that cannot be reached being reported as success: every request ends with no
/// status, the pass counts them as failures, and the status counts say `none`.
#[tokio::test]
async fn requests_that_get_no_answer_are_failures_with_no_status() {
    let mut r = rig(
        "tierstore-bench-cp-down",
        &["--in-flight", "2", "--puts", "4", "--object-bytes", "64"],
    )
    .await;
    // Point the cell at a port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    r.ctx
        .config
        .stores
        .get_mut(&tierstore_bench::role::Role::Cold)
        .unwrap()
        .endpoint = endpoint;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    let pass = &run.results[0].passes[0];
    assert_eq!(pass.requests, 4);
    assert_eq!(pass.failures, 4);
    // Nothing was stored, so no bytes and no percentiles; the attempts are in the notes.
    assert_eq!(pass.bytes, 0);
    assert!(pass.percentiles.is_none());
    assert!(
        run.results[0]
            .notes
            .iter()
            .any(|n| n.contains("attempted by every request: 256")),
        "{:?}",
        run.results[0].notes
    );
    assert!(
        run.results[0].notes.iter().any(|n| n.contains("none=4")),
        "{:?}",
        run.results[0].notes
    );
    let rows = r.csv_rows();
    assert!(rows.iter().all(|row| row[13].is_empty()));
}

/// Catches a run id or prefix that could name another run's keys or leave the measurement
/// prefix: each is refused before any request, with nothing logged.
#[tokio::test]
async fn a_run_id_or_prefix_that_could_leave_the_measurement_prefix_is_refused() {
    let cases = [
        ("", "bench/c1"),
        ("a/b", "bench/c1"),
        ("..", "bench/c1"),
        ("r 1", "bench/c1"),
        ("r1", ""),
        ("r1", "/bench"),
        ("r1", "bench/../cas"),
    ];
    for (n, (run_id, prefix)) in cases.into_iter().enumerate() {
        let r = rig(
            &format!("tierstore-bench-cp-names-{n}"),
            &["--puts", "2", "--prefix", prefix],
        )
        .await;
        let mut plan = r.plan.clone();
        plan.cold_put.run_id = run_id.to_owned();
        let err = cold_put::run_levels(&r.ctx, &plan, Script::calm())
            .await
            .unwrap_err();
        assert!(
            matches!(err, BenchError::Request(_)),
            "{run_id:?} {prefix:?}"
        );
        assert_eq!(r.ctx.log.count(), 0);
        assert!(r.fake.keys().is_empty());
    }
}

/// Catches a same-key run that is not one race: sixteen writers create one key, exactly one is
/// answered 200 and fifteen 412, the stored object has the winner's length and bytes, and
/// the key is the only object written.
#[tokio::test]
async fn sixteen_concurrent_creates_of_one_key_have_one_winner() {
    let r = rig(
        "tierstore-bench-cp-samekey",
        &["--same-key", "--in-flight", "16", "--object-bytes", "2048"],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted, None);
    assert_eq!(r.fake.keys(), ["bench/c1/r1/same-key"]);
    let report = run.same_key.expect("a same-key report");
    assert_eq!(report.writers, 16);
    assert_eq!(report.statuses.get(&Some(200)), Some(&1));
    assert_eq!(report.statuses.get(&Some(412)), Some(&15));
    assert_eq!(report.statuses.len(), 2);
    assert_eq!(report.winners.len(), 1);
    assert_eq!(report.stored_len, Some(2048));
    assert_eq!(report.equal_to, report.winners);
    assert_eq!(report.stored_equals_winner, Some(true));

    let pass = &run.results[0].passes[0];
    assert_eq!(run.results[0].arm, "direct-same-key-c16");
    assert_eq!(pass.requests, 16);
    assert_eq!(pass.failures, 0);
    // One object was stored, not sixteen; every request attempted 2048 bytes.
    assert_eq!(pass.bytes, 2048);
    let notes = run.results[0].notes.join("\n");
    assert!(
        notes.contains("attempted by every request: 32768"),
        "{notes}"
    );
    assert!(
        notes.contains("200=1") && notes.contains("412=15"),
        "{notes}"
    );
    assert!(
        notes.contains("stored bytes equal the winner's: yes"),
        "{notes}"
    );
    let rows = r.csv_rows();
    assert_eq!(rows.len(), 16);
    // The percentiles are the one PUT the store took, not the fifteen refusals.
    let winner: Vec<u64> = rows
        .iter()
        .filter(|row| row[13] == "200")
        .map(|row| row[12].parse().unwrap())
        .collect();
    let p = pass.percentiles.expect("percentiles");
    assert_eq!(winner.len(), 1);
    assert_eq!(
        (p.p50_us, p.p95_us, p.max_us),
        (winner[0], winner[0], winner[0])
    );
    assert!(notes.contains("answered 200 only"), "{notes}");
}

/// Catches the percentiles of a level mixing in the 412s: with one refused key among eight, the
/// percentiles are those of the seven PUTs the store took, and `bytes` counts the seven
/// objects stored, not the eight attempted.
#[tokio::test]
async fn percentiles_and_bytes_are_those_of_the_puts_the_store_took() {
    let r = rig(
        "tierstore-bench-cp-headline",
        &["--in-flight", "2", "--puts", "8", "--object-bytes", "256"],
    )
    .await;
    r.put_elsewhere("bench/c1/r1/c2/00000005").await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    let pass = &run.results[0].passes[0];
    let mut ok: Vec<u64> = r
        .csv_rows()
        .iter()
        .filter(|row| row[13] == "200")
        .map(|row| row[12].parse().unwrap())
        .collect();
    ok.sort_unstable();
    assert_eq!(ok.len(), 7);
    let p = pass.percentiles.expect("percentiles");
    assert_eq!(p.max_us, ok[6]);
    assert_eq!(p.p50_us, ok[3]);
    assert_eq!(pass.bytes, 7 * 256);
    let notes = run.results[0].notes.join("\n");
    assert!(
        notes.contains("attempted by every request: 2048"),
        "{notes}"
    );
    assert!(notes.contains("7 PUTs answered 200 only"), "{notes}");
}

/// Catches a level whose bodies could not fit in memory: in flight x object bytes over 1 GiB
/// is refused before any request, for a level and for the same-key race; exactly 1 GiB is not.
#[tokio::test]
async fn in_flight_times_object_size_over_a_gibibyte_is_refused_before_any_request() {
    let r = rig(
        "tierstore-bench-cp-bound",
        &["--in-flight", "4,16", "--object-bytes", "134217728"],
    )
    .await;
    let err = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Request(_)), "{err:?}");
    let race = rig(
        "tierstore-bench-cp-bound-race",
        &[
            "--same-key",
            "--in-flight",
            "17",
            "--object-bytes",
            "67108864",
        ],
    )
    .await;
    let err = cold_put::run_levels(&race.ctx, &race.plan, Script::calm())
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Request(_)), "{err:?}");
    for refused in [&r, &race] {
        assert_eq!(refused.ctx.log.count(), 0);
        assert!(refused.fake.keys().is_empty());
    }
    // 8 x 128 MiB is exactly the bound and is allowed to start (no object is sent here: the
    // stop file is up).
    let edge = rig(
        "tierstore-bench-cp-bound-edge",
        &[
            "--in-flight",
            "8",
            "--object-bytes",
            "134217728",
            "--puts",
            "1",
        ],
    )
    .await;
    let stop = edge.dir.path().join("stop");
    std::fs::write(&stop, "").unwrap();
    let mut plan = edge.plan.clone();
    plan.cold_put.stop_file = Some(stop);
    let run = cold_put::run_levels(&edge.ctx, &plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted.as_deref(), Some("stop-file"));
}

/// Catches a load that cannot be read being taken for a calm machine: at the start no request is
/// sent and the run is halted with that reason; with no measure named as tripped.
#[tokio::test]
async fn a_load_that_cannot_be_read_at_the_start_halts_the_run() {
    let r = rig("tierstore-bench-cp-unread-start", &["--in-flight", "4,8"]).await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Unreadable::after(0))
        .await
        .unwrap();
    assert_eq!(run.halted.as_deref(), Some("load figures unreadable"));
    assert_eq!(run.results.len(), 1);
    let pass = &run.results[0].passes[0];
    assert_eq!((pass.requests, pass.gate_tripped.clone()), (0, None));
    let notes = run.results[0].notes.join("\n");
    assert!(
        notes.contains("ended by: load figures unreadable, before any request"),
        "{notes}"
    );
    assert!(r.fake.keys().is_empty());
}

/// Catches the gate being read once and trusted: a load that is readable at the start and not
/// after the first batch ends the level and the run, and the end-of-level reading, which also
/// fails, is left out of the notes rather than invented.
#[tokio::test]
async fn a_load_that_becomes_unreadable_between_batches_halts_the_run() {
    let r = rig(
        "tierstore-bench-cp-unread-mid",
        &[
            "--in-flight",
            "4,8",
            "--puts",
            "400",
            "--gate-every",
            "10",
            "--object-bytes",
            "64",
        ],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Unreadable::after(1))
        .await
        .unwrap();
    assert_eq!(run.halted.as_deref(), Some("load figures unreadable"));
    assert_eq!(run.results.len(), 1);
    let pass = &run.results[0].passes[0];
    assert!((10..400).contains(&pass.requests), "{}", pass.requests);
    let notes = run.results[0].notes.join("\n");
    assert!(notes.contains("load at start"), "{notes}");
    assert!(!notes.contains("load at end"), "{notes}");
}

/// Catches a stop file that is looked for only at the start: one that appears while a level
/// runs ends the level before the next put, and the run is halted by it.
#[tokio::test]
async fn a_stop_file_that_appears_during_a_level_halts_the_run() {
    let dir = TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        "tierstore-bench-cp-stop-mid-dir",
    );
    let stop = dir.path().join("stop");
    let r = rig(
        "tierstore-bench-cp-stop-mid",
        &[
            "--in-flight",
            "4",
            "--puts",
            "400",
            "--gate-every",
            "10",
            "--object-bytes",
            "64",
            "--stop-file",
            stop.to_str().unwrap(),
        ],
    )
    .await;
    // The second reading of the load, the first inside the level, puts the file down.
    let run = cold_put::run_levels(&r.ctx, &r.plan, TouchOn::new(stop.clone(), 2))
        .await
        .unwrap();
    assert!(stop.exists());
    assert_eq!(run.halted.as_deref(), Some("stop-file"));
    let pass = &run.results[0].passes[0];
    assert!((10..400).contains(&pass.requests), "{}", pass.requests);
}

/// Catches a level that starts with a byte budget too small for one object: no request, the
/// reason in the notes, and the run halted by the byte cap rather than a silent empty pass.
#[tokio::test]
async fn a_byte_budget_that_holds_no_object_ends_the_run_before_any_put() {
    let mut r = rig(
        "tierstore-bench-cp-nobudget",
        &["--in-flight", "2,4", "--object-bytes", "1000"],
    )
    .await;
    r.ctx.config.bytes.pass = 100;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted.as_deref(), Some("byte-cap"));
    assert_eq!(run.results.len(), 1);
    assert_eq!(run.results[0].passes[0].requests, 0);
    let notes = run.results[0].notes.join("\n");
    assert!(notes.contains("holds less than one object"), "{notes}");
    assert!(r.fake.keys().is_empty());
}

/// Catches the concurrent-create check starting without room for a body per writer, and a
/// load gate that is not read for it: each ends it before any request, with its reason.
#[tokio::test]
async fn the_same_key_check_does_not_start_without_room_or_on_a_busy_machine() {
    let mut r = rig(
        "tierstore-bench-sk-nobudget",
        &["--same-key", "--in-flight", "4", "--object-bytes", "1000"],
    )
    .await;
    r.ctx.config.bytes.pass = 3999;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted.as_deref(), Some("byte-cap"));
    assert!(run.same_key.is_none());
    let notes = run.results[0].notes.join("\n");
    assert!(notes.contains("less than one object per writer"), "{notes}");
    assert!(r.fake.keys().is_empty());

    let busy = rig(
        "tierstore-bench-sk-busy",
        &["--same-key", "--in-flight", "4", "--object-bytes", "100"],
    )
    .await;
    let run = cold_put::run_levels(&busy.ctx, &busy.plan, Script::new([busy_load()]))
        .await
        .unwrap();
    assert!(
        run.halted
            .as_deref()
            .is_some_and(|h| h.contains("loadavg_1m")),
        "{:?}",
        run.halted
    );
    assert!(run.same_key.is_none());
    assert_eq!(
        run.results[0].passes[0].gate_tripped.as_deref(),
        Some("loadavg_1m")
    );
    assert!(busy.fake.keys().is_empty());
}

/// Catches a store that cannot be reached being reported as having a winner or a stored object:
/// all writers get no answer, no stored object is found, and the notes say so.
#[tokio::test]
async fn a_same_key_check_against_a_store_that_is_gone_reports_no_winner_and_no_object() {
    let mut r = rig(
        "tierstore-bench-sk-gone",
        &["--same-key", "--in-flight", "4", "--object-bytes", "64"],
    )
    .await;
    r.store_goes_away();
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    let report = run.same_key.expect("a report");
    assert_eq!(report.statuses.get(&None), Some(&4));
    assert_eq!(report.statuses.len(), 1);
    assert!(report.winners.is_empty());
    assert_eq!(report.stored_len, None);
    assert!(report.equal_to.is_empty());
    assert_eq!(report.stored_equals_winner, None);
    let pass = &run.results[0].passes[0];
    assert_eq!((pass.failures, pass.bytes), (4, 0));
    let notes = run.results[0].notes.join("\n");
    assert!(
        notes.contains("stored length (HEAD after the race): none"),
        "{notes}"
    );
    assert!(notes.contains("n/a (0 winners here)"), "{notes}");
}

/// Catches the shared start time being ignored or breaking a start time already past: with a
/// start in the past the writers go at once, with one a little ahead they go after it, and
/// either way one writer wins.
#[tokio::test]
async fn the_same_key_writers_can_be_released_at_a_given_time() {
    for (n, start) in [1u64, unix_ms() + 30].into_iter().enumerate() {
        let r = rig(
            &format!("tierstore-bench-sk-start-{n}"),
            &[
                "--same-key",
                "--in-flight",
                "4",
                "--object-bytes",
                "64",
                "--start-at-unix-ms",
                &start.to_string(),
            ],
        )
        .await;
        let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
            .await
            .unwrap();
        let report = run.same_key.expect("a report");
        assert_eq!(report.winners.len(), 1, "start {start}");
        assert_eq!(report.stored_equals_winner, Some(true));
    }
}

/// Catches a request log that cannot be written being survived: the run stops with the log's
/// error, for a level and for the concurrent-create check.
#[tokio::test]
async fn a_request_log_that_cannot_be_written_stops_the_cell() {
    for (n, extra) in [
        vec!["--in-flight", "2", "--puts", "4", "--object-bytes", "8"],
        vec!["--same-key", "--in-flight", "2", "--object-bytes", "8"],
    ]
    .into_iter()
    .enumerate()
    {
        let mut r = rig(&format!("tierstore-bench-cp-logfail-{n}"), &extra).await;
        r.ctx.log = Arc::new(RequestLog::to_writer(Refusing));
        let err = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Log(_)), "{err:?}");
    }
}

/// Catches plans the cell must refuse before any request: a size of zero or several sizes, no
/// level or a level of zero, and several levels for the concurrent-create check.
#[tokio::test]
async fn a_plan_with_a_bad_size_or_level_is_refused_before_any_request() {
    let r = rig("tierstore-bench-cp-badplan", &["--puts", "2"]).await;
    let mut plans = Vec::new();
    for sizes in [vec![0], vec![8, 16]] {
        let mut p = r.plan.clone();
        p.object_bytes = sizes;
        plans.push(p);
    }
    for levels in [vec![], vec![2, 0]] {
        let mut p = r.plan.clone();
        p.in_flight = levels;
        plans.push(p);
    }
    let mut p = r.plan.clone();
    p.in_flight = vec![2, 4];
    p.cold_put.same_key = true;
    plans.push(p);
    for plan in &plans {
        let err = cold_put::run_levels(&r.ctx, plan, Script::calm())
            .await
            .unwrap_err();
        assert!(matches!(err, BenchError::Request(_)), "{err:?}");
    }
    assert_eq!(r.ctx.log.count(), 0);
    assert!(r.fake.keys().is_empty());
}

/// Catches a run id given by the context being overwritten by the plan's: the CSV carries the
/// provenance the caller gave. Also a level time cap too large to add to the clock is no cap.
#[tokio::test]
async fn a_given_provenance_is_kept_and_a_huge_time_cap_is_no_cap() {
    let mut r = rig(
        "tierstore-bench-cp-prov",
        &[
            "--in-flight",
            "2",
            "--puts",
            "4",
            "--object-bytes",
            "8",
            "--level-secs",
            "18446744073709551615",
        ],
    )
    .await;
    r.ctx.provenance.run_id = "given".to_owned();
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    assert_eq!(run.halted, None);
    assert_eq!(run.results[0].passes[0].requests, 4);
    assert_eq!(run.results[0].provenance.run_id, "given");
    assert!(r.csv_rows().iter().all(|row| row[0] == "given"));
}

/// Catches the p99 being claimed from too little or from a busy window, or never: 1,000 puts in
/// a window declared quiet give a `p99_us` in the result, at least as large as the median.
#[tokio::test]
async fn a_quiet_window_with_a_thousand_puts_claims_a_p99() {
    let r = rig(
        "tierstore-bench-cp-p99",
        &[
            "--in-flight",
            "16",
            "--puts",
            "1000",
            "--object-bytes",
            "8",
            "--window",
            "quiet",
            "--gate-every",
            "0",
        ],
    )
    .await;
    let run = cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    let pass = &run.results[0].passes[0];
    assert_eq!((pass.requests, pass.window), (1000, Window::Quiet));
    let p = pass.percentiles.expect("percentiles");
    assert!(p.p99_us.expect("a p99") >= p.p50_us);
    assert!(p.max_us >= p.p99_us.unwrap());
}

/// Catches the cell run through `cells::run` not reading the machine's load from the tree it is
/// pointed at, and not stopping on a busy one: on a calm made-up `/proc` the levels run and the
/// results come back; on one with a high load average the run is a halted error naming it.
#[tokio::test]
async fn the_cell_reads_the_load_from_the_proc_it_is_given() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-cp-proc");
    let (calm_proc, sys) = proc_tree(&dir.path().join("calm"), "0.50");
    let (busy_proc, _) = proc_tree(&dir.path().join("busy"), "99.00");
    let args = |proc: &std::path::Path| {
        vec![
            "--in-flight".to_owned(),
            "2".to_owned(),
            "--puts".to_owned(),
            "4".to_owned(),
            "--object-bytes".to_owned(),
            "8".to_owned(),
            "--proc-dir".to_owned(),
            proc.to_str().unwrap().to_owned(),
            "--sys-block-dir".to_owned(),
            sys.to_str().unwrap().to_owned(),
        ]
    };
    let calm_args = args(&calm_proc);
    let r = rig(
        "tierstore-bench-cp-proc-calm",
        &calm_args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    let results = cells::run(CellId::ColdPut, &r.ctx, &r.plan).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].passes[0].requests, 4);
    let notes = results[0].notes.join("\n");
    assert!(notes.contains("psi_io_avg10=1.50"), "{notes}");

    let busy_args = args(&busy_proc);
    let b = rig(
        "tierstore-bench-cp-proc-busy",
        &busy_args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await;
    match cells::run(CellId::ColdPut, &b.ctx, &b.plan).await {
        Err(BenchError::Halted(why)) => assert!(why.contains("loadavg_1m"), "{why}"),
        other => panic!("expected a halt, got {other:?}"),
    }
    assert!(b.fake.keys().is_empty());
}
