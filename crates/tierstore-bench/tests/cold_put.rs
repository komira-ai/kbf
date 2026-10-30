//! The cold-put cell against a loopback S3 service: what a level writes, what it reports, and
//! when it stops. No test asserts a duration; caps are driven by a count, a byte budget, a zero
//! time budget or a scripted load source.

mod common;

use common::{Script, calm, rig};
use kbf_testkit::dir::TestDir;
use tierstore_bench::cells::{CellId, cold_put};
use tierstore_bench::error::BenchError;
use tierstore_bench::gate::LoadSample;
use tierstore_bench::result::{CellResult, Window};

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
    let notes = run.results[0].notes.join("\n");
    assert!(
        notes.contains("200=1") && notes.contains("412=15"),
        "{notes}"
    );
    assert!(
        notes.contains("stored bytes equal the winner's: yes"),
        "{notes}"
    );
    assert_eq!(r.csv_rows().len(), 16);
}
