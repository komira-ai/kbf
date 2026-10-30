//! `cleanup` against a loopback S3 service: it deletes the keys the request log lists as written
//! by this tool, and nothing else.

mod common;

use common::{BUCKET, Script, rig};
use tierstore_bench::cells::cold_put;
use tierstore_bench::cleanup;
use tierstore_bench::error::BenchError;
use tierstore_bench::requestlog::RequestRecord;
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
