//! `cleanup` against a loopback S3 service: it deletes the keys the request log lists as written
//! by this tool, and nothing else.

mod common;

use std::sync::Arc;

use clap::Parser as _;
use common::{BUCKET, Refusing, Script, rig};
use tierstore_bench::cells::cold_put;
use tierstore_bench::cleanup;
use tierstore_bench::cli::{self, Cli};
use tierstore_bench::error::BenchError;
use tierstore_bench::requestlog::{RequestLog, RequestRecord};
use tierstore_bench::role::Role;

fn put_record(key: &str, status: Option<u16>) -> RequestRecord {
    RequestRecord {
        unix_ms: 1,
        role: Role::Cold,
        method: "PUT".to_owned(),
        bucket: BUCKET.to_owned(),
        key: key.to_owned(),
        status,
        latency_us: 10,
        bytes: 8,
        error_kind: status.is_none().then(|| "transport".to_owned()),
    }
}

/// Writes `records` as the request log of the rig, replacing what is there.
fn write_log(out: &std::path::Path, records: &[RequestRecord]) {
    let mut text = String::new();
    for record in records {
        text.push_str(&serde_json::to_string(record).unwrap());
        text.push('\n');
    }
    std::fs::write(out.join("requests.jsonl"), text).unwrap();
}

/// Catches a cleanup that deletes by listing the prefix instead of by the log: after a level,
/// an object another client put under the same run prefix and an object of another run stay,
/// every object the level wrote is gone, the counts say written 6 deleted 6, the listing still
/// shows the one object that is not ours, and the deletes are in the log.
#[tokio::test]
async fn cleanup_deletes_exactly_the_logged_keys_and_nothing_else_under_the_prefix() {
    let r = rig(
        "tierstore-bench-cl-exact",
        &["--in-flight", "2", "--puts", "6", "--object-bytes", "128"],
    )
    .await;
    cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    r.put_elsewhere("bench/c1/r1/other").await;
    r.put_elsewhere("bench/c1/r2/c2/00000000").await;
    assert_eq!(r.fake.keys().len(), 8);

    let report = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 4)
        .await
        .unwrap();
    assert_eq!(report.put_ok, 6);
    assert_eq!(report.put_unanswered, 0);
    assert_eq!(report.deleted, 6);
    assert_eq!(report.failed, 0);
    assert_eq!(report.remaining, 1);
    assert_eq!(
        r.fake.keys(),
        ["bench/c1/r1/other", "bench/c1/r2/c2/00000000"]
    );
    let deletes = r
        .log_records()
        .iter()
        .filter(|x| x.method == "DELETE")
        .count();
    assert_eq!(deletes, 6);

    // Run again: the log still lists the six, the deletes of absent keys are acknowledged, the
    // other objects are still untouched.
    let again = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 4)
        .await
        .unwrap();
    assert_eq!((again.put_ok, again.deleted, again.failed), (6, 6, 0));
    assert_eq!(r.fake.keys().len(), 2);
}

/// Catches the three wrong readings of a log: a 412 is a key somebody else wrote (not ours), a
/// refused put (503) wrote nothing, and a put that got no answer may have landed (so it is
/// deleted); a HEAD, another bucket's put and an unlogged object are never deleted.
#[tokio::test]
async fn only_acknowledged_and_unanswered_puts_are_ours() {
    let r = rig("tierstore-bench-cl-status", &[]).await;
    for key in ["a", "b", "c", "d", "e"] {
        r.put_elsewhere(&format!("bench/c1/r1/{key}")).await;
    }
    let mut elsewhere = put_record("bench/c1/r1/e", Some(200));
    elsewhere.bucket = "another-bucket".to_owned();
    let mut head = put_record("bench/c1/r1/e", Some(200));
    head.method = "HEAD".to_owned();
    write_log(
        &r.out,
        &[
            put_record("bench/c1/r1/a", Some(200)),
            put_record("bench/c1/r1/b", Some(412)),
            put_record("bench/c1/r1/c", Some(503)),
            put_record("bench/c1/r1/d", None),
            head,
            elsewhere,
        ],
    );
    let report = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap();
    assert_eq!(report.put_ok, 1);
    assert_eq!(report.put_unanswered, 1);
    assert_eq!(report.deleted, 2);
    assert_eq!(
        r.fake.keys(),
        ["bench/c1/r1/b", "bench/c1/r1/c", "bench/c1/r1/e"]
    );
    assert_eq!(report.remaining, 3);
}

/// Catches a cleanup that trusts a log naming a key outside the run's prefix: it refuses before
/// deleting anything, so the key that is inside the prefix stays too.
#[tokio::test]
async fn a_log_that_names_a_key_outside_the_run_prefix_is_refused_before_any_delete() {
    let r = rig("tierstore-bench-cl-outside", &[]).await;
    r.put_elsewhere("bench/c1/r1/c2/00000000").await;
    r.put_elsewhere("cas/b/deadbeef").await;
    write_log(
        &r.out,
        &[
            put_record("bench/c1/r1/c2/00000000", Some(200)),
            put_record("cas/b/deadbeef", Some(200)),
        ],
    );
    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Request(_)), "{err:?}");
    assert_eq!(r.fake.keys().len(), 2);
    assert_eq!(r.ctx.log.count(), 0);
}

/// Catches a cleanup that reports success without a log: with no request log there is nothing
/// it may delete, and it says so.
#[tokio::test]
async fn a_missing_log_is_an_error_not_an_empty_success() {
    let r = rig("tierstore-bench-cl-nolog", &[]).await;
    std::fs::remove_file(r.out.join("requests.jsonl")).unwrap();
    assert!(matches!(
        cleanup::open_log(&r.ctx.config),
        Err(BenchError::Io { .. })
    ));
    assert!(!r.out.join("requests.jsonl").exists());
    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Io { .. }), "{err:?}");
}

/// Catches a cleanup that leaves the winner of a same-key race behind, or deletes more than the
/// one key: the winner's 200 is in the log, the 412s are not, and the key is gone.
#[tokio::test]
async fn cleanup_after_a_same_key_run_deletes_the_one_object_the_winner_wrote() {
    let r = rig(
        "tierstore-bench-cl-samekey",
        &["--same-key", "--in-flight", "8", "--object-bytes", "256"],
    )
    .await;
    cold_put::run_levels(&r.ctx, &r.plan, Script::calm())
        .await
        .unwrap();
    r.put_elsewhere("bench/c1/r1/other").await;
    assert_eq!(r.fake.keys().len(), 2);
    let report = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap();
    assert_eq!((report.put_ok, report.deleted, report.failed), (1, 1, 0));
    assert_eq!(r.fake.keys(), ["bench/c1/r1/other"]);
}

/// Catches a per-node namespace that does not work or leaks into its neighbour's: a three-segment
/// prefix (`bench/c1/<run>`) with run id `n01` is accepted, and a cleanup of `n01` deletes the
/// key its log lists and leaves `n010`'s key, which shares the characters `n01` but not the
/// prefix `bench/c1/<run>/n01/`.
#[tokio::test]
async fn a_node_namespace_cleanup_does_not_touch_the_node_whose_id_starts_the_same() {
    assert_eq!(
        cold_put::run_prefix("bench/c1/r1", "n01").unwrap(),
        "bench/c1/r1/n01/"
    );
    let r = rig("tierstore-bench-cl-ns", &[]).await;
    for key in [
        "bench/c1/r1/n01/c4/00000000",
        "bench/c1/r1/n010/c4/00000000",
        "bench/c1/r1/n02/c4/00000000",
    ] {
        r.put_elsewhere(key).await;
    }
    write_log(
        &r.out,
        &[put_record("bench/c1/r1/n01/c4/00000000", Some(200))],
    );
    let report = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1/r1", "n01", 2)
        .await
        .unwrap();
    assert_eq!((report.put_ok, report.deleted, report.failed), (1, 1, 0));
    assert_eq!(report.remaining, 0);
    assert_eq!(
        r.fake.keys(),
        [
            "bench/c1/r1/n010/c4/00000000",
            "bench/c1/r1/n02/c4/00000000"
        ]
    );
    // A log that lists n010's key is refused by the n01 cleanup, whole.
    write_log(
        &r.out,
        &[put_record("bench/c1/r1/n010/c4/00000000", Some(200))],
    );
    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1/r1", "n01", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Request(_)), "{err:?}");
    assert_eq!(r.fake.keys().len(), 2);
}

/// Catches a cleanup that carries on when its own log of deletes cannot be written: it stops with
/// that error rather than delete what it cannot record.
#[tokio::test]
async fn a_log_that_cannot_record_the_deletes_stops_the_cleanup() {
    let r = rig("tierstore-bench-cl-logfail", &[]).await;
    r.put_elsewhere("bench/c1/r1/a").await;
    write_log(&r.out, &[put_record("bench/c1/r1/a", Some(200))]);
    let log = Arc::new(RequestLog::to_writer(Refusing));
    let err = cleanup::run(&r.ctx.config, &log, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Log(_)), "{err:?}");
}

/// Catches deletes that got no answer being counted as deleted, or the run being lost for it: the
/// report counts them as failed, says the listing failed too, and halts with both in words.
#[tokio::test]
async fn deletes_and_a_listing_that_get_no_answer_are_in_the_report_not_a_lost_run() {
    let mut r = rig("tierstore-bench-cl-gone", &[]).await;
    write_log(
        &r.out,
        &[
            put_record("bench/c1/r1/a", Some(200)),
            put_record("bench/c1/r1/b", None),
            put_record("bench/c1/r1/c", Some(200)),
        ],
    );
    r.store_goes_away();
    let report = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap();
    assert_eq!(
        (
            report.put_ok,
            report.put_unanswered,
            report.deleted,
            report.failed
        ),
        (2, 1, 0, 3)
    );
    assert!(report.list_failed);
    assert!(
        report
            .summary_line()
            .ends_with("remaining_under_prefix=unknown")
    );
    let why = report.halted_reason().expect("incomplete");
    assert!(why.starts_with("3 deletes"), "{why}");
}

/// Catches a count of what is left that stops at the first page of the listing: 1,001 objects
/// under the run prefix that the log does not list are all counted, over two pages.
#[tokio::test]
async fn what_is_left_is_counted_over_every_page_of_the_listing() {
    let r = rig("tierstore-bench-cl-pages", &[]).await;
    r.put_many_elsewhere("bench/c1/r1/n02/", 1001).await;
    write_log(&r.out, &[]);
    let report = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 4)
        .await
        .unwrap();
    assert_eq!(
        (report.written(), report.deleted, report.remaining),
        (0, 0, 1001)
    );
    assert!(report.halted_reason().is_none());
}

fn run_file_for(r: &common::Rig, endpoint: &str) -> std::path::PathBuf {
    let path = r.dir.path().join("cli-run.json");
    std::fs::write(
        &path,
        format!(
            r#"{{"stores": {{"cold": {{"endpoint": "{endpoint}", "bucket": "{BUCKET}",
                 "access_key_id": "k", "secret_access_key": "s"}}}}}}"#
        ),
    )
    .unwrap();
    path
}

async fn cli_cleanup(r: &common::Rig, run_file: &std::path::Path) -> Result<(), BenchError> {
    let argv = [
        "tierstore-bench",
        "cleanup",
        "--result-dir",
        r.out.to_str().unwrap(),
        "--run-file",
        run_file.to_str().unwrap(),
        "--run-id",
        "r1",
    ];
    cli::run(Cli::try_parse_from(argv).unwrap()).await
}

/// Catches a cleanup that starts on a configuration or a log it cannot trust: a run id that
/// could name another run's keys, no cold store, a log with a line in the middle that is not a
/// record, a bucket the client refuses, and (with nothing to delete) a log of deletes that
/// cannot be written when the listing is recorded. Each is an error, and nothing is deleted.
#[tokio::test]
async fn a_cleanup_that_cannot_start_or_record_is_an_error_and_deletes_nothing() {
    let mut r = rig("tierstore-bench-cl-refuse", &[]).await;
    r.put_elsewhere("bench/c1/r1/a").await;
    write_log(&r.out, &[put_record("bench/c1/r1/a", Some(200))]);

    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "a/b", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Request(_)), "{err:?}");

    let good = serde_json::to_string(&put_record("bench/c1/r1/a", Some(200))).unwrap();
    std::fs::write(
        r.out.join("requests.jsonl"),
        format!("{good}\nnot a record\n{good}\n"),
    )
    .unwrap();
    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Result(_)), "{err:?}");

    write_log(&r.out, &[]);
    let refusing = Arc::new(RequestLog::to_writer(Refusing));
    let err = cleanup::run(&r.ctx.config, &refusing, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Log(_)), "{err:?}");

    r.ctx.config.stores.get_mut(&Role::Cold).unwrap().bucket = "B".to_owned();
    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Store(_)), "{err:?}");

    r.ctx.config.stores.clear();
    let err = cleanup::run(&r.ctx.config, &r.ctx.log, "bench/c1", "r1", 2)
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Config(_)), "{err:?}");
    assert_eq!(r.fake.keys(), ["bench/c1/r1/a"]);
}

/// Catches the verbs hiding a bad configuration: a run file that is not there is a config error
/// for `cleanup` and for `run`, and a run id that could leave its prefix is refused by `cleanup`.
#[tokio::test]
async fn the_verbs_refuse_a_missing_run_file_and_a_bad_run_id() {
    let r = rig("tierstore-bench-cl-verb-bad", &[]).await;
    write_log(&r.out, &[]);
    let out = r.out.to_str().unwrap();
    let ok_file = r.dir.path().join("run.json");
    let missing = r.dir.path().join("none.json");
    let cases: [Vec<&str>; 3] = [
        vec![
            "cleanup",
            "--result-dir",
            out,
            "--run-file",
            missing.to_str().unwrap(),
            "--run-id",
            "r1",
        ],
        vec![
            "cleanup",
            "--result-dir",
            out,
            "--run-file",
            ok_file.to_str().unwrap(),
            "--run-id",
            ".x",
        ],
        vec![
            "run",
            "--result-dir",
            out,
            "--run-file",
            missing.to_str().unwrap(),
            "--cell",
            "cold-put",
        ],
    ];
    let want_config = [true, false, true];
    for (argv, config) in cases.into_iter().zip(want_config) {
        let mut full = vec!["tierstore-bench"];
        full.extend(argv);
        let err = cli::run(Cli::try_parse_from(full).unwrap())
            .await
            .unwrap_err();
        if config {
            assert!(matches!(err, BenchError::Config(_)), "{err:?}");
        } else {
            assert!(matches!(err, BenchError::Request(_)), "{err:?}");
        }
    }
}

/// Catches the `cleanup` verb reporting success when it is not, and failing when it is: with the
/// store reachable it deletes the logged key and succeeds; with the store gone it fails as a
/// halt that names the deletes not acknowledged.
#[tokio::test]
async fn the_cleanup_verb_succeeds_when_complete_and_halts_when_not() {
    let r = rig("tierstore-bench-cl-verb", &[]).await;
    r.put_elsewhere("bench/c1/r1/a").await;
    write_log(&r.out, &[put_record("bench/c1/r1/a", Some(200))]);
    let reachable = run_file_for(&r, r.fake.endpoint());
    cli_cleanup(&r, &reachable).await.unwrap();
    assert!(r.fake.keys().is_empty());

    write_log(&r.out, &[put_record("bench/c1/r1/a", Some(200))]);
    let gone = run_file_for(&r, "http://127.0.0.1:9");
    match cli_cleanup(&r, &gone).await {
        Err(BenchError::Halted(why)) => assert!(why.starts_with("1 deletes"), "{why}"),
        other => panic!("expected a halt, got {other:?}"),
    }
}
