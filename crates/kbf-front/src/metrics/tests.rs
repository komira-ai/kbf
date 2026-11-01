use std::time::Duration;

use kbf_meta::{Generation, ObjectId, Touch, UnreachableReason};
use kbf_types::FarmTime;

use super::*;

/// The lines of `text` that start with `prefix`.
fn lines<'a>(text: &'a str, prefix: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.starts_with(prefix)).collect()
}

/// Catches a value on a bound counted in the bucket above it (Prometheus's `le` is
/// "less than or equal"), buckets printed per bucket instead of cumulatively, a
/// `_count` that is not the `+Inf` bucket, and a sum printed in nanoseconds.
#[test]
fn a_histogram_prints_cumulative_buckets_in_seconds() {
    let h = Histogram::new(Unit::Seconds);
    h.observe(1_000_000);
    h.observe(1_000_001);
    h.observe_duration(Duration::from_secs(90));
    let mut out = String::new();
    h.render(&mut out, "x", "");
    let buckets = lines(&out, "x_bucket");
    assert_eq!(buckets.len(), SECONDS_BOUNDS.len() + 1);
    assert_eq!(buckets[0], r#"x_bucket{le="0.001"} 1"#);
    assert_eq!(buckets[1], r#"x_bucket{le="0.0025"} 2"#);
    assert_eq!(buckets[14], r#"x_bucket{le="60"} 2"#);
    assert_eq!(buckets[15], r#"x_bucket{le="+Inf"} 3"#);
    assert_eq!(lines(&out, "x_sum"), ["x_sum 90.002000001"]);
    assert_eq!(lines(&out, "x_count"), ["x_count 3"]);
    assert_eq!((h.count(), h.sum()), (3, 90_002_000_001));
}

/// Catches a label printed without its comma before `le`, or `_sum` and `_count`
/// without the label (two series of one family would then collide), and sizes or
/// counts printed as seconds.
#[test]
fn labels_and_plain_units_print_as_prometheus_reads_them() {
    let h = Histogram::new(Unit::Bytes);
    h.observe(1024);
    h.observe(5000);
    let mut out = String::new();
    h.render(&mut out, "y", r#"command="touch""#);
    assert_eq!(
        lines(&out, "y_bucket")[..3],
        [
            r#"y_bucket{command="touch",le="1024"} 1"#,
            r#"y_bucket{command="touch",le="4096"} 1"#,
            r#"y_bucket{command="touch",le="16384"} 2"#,
        ]
    );
    assert_eq!(lines(&out, "y_sum"), [r#"y_sum{command="touch"} 6024"#]);
    assert_eq!(lines(&out, "y_count"), [r#"y_count{command="touch"} 2"#]);
    let c = Histogram::new(Unit::Count);
    c.observe(3);
    let mut out = String::new();
    c.render(&mut out, "z", "");
    assert_eq!(lines(&out, "z_bucket")[1], r#"z_bucket{le="4"} 1"#);
    assert_eq!(lines(&out, "z_sum"), ["z_sum 3"]);
}

/// Catches a request guard that does not lower the in-flight level when dropped (an
/// error return or a cancelled call would leave it raised for ever), a request
/// counted only when it succeeds, and one that succeeded counted as an error.
#[test]
fn a_request_is_counted_however_it_ends() {
    let requests = Requests::default();
    {
        let mut ok = requests.start();
        let failed = requests.start();
        assert_eq!(requests.in_flight.get(), 2);
        ok.bytes(7);
        failed.bytes(5);
        ok.ok();
    }
    assert_eq!(requests.in_flight.get(), 0);
    assert_eq!(requests.total.get(), 2);
    assert_eq!(requests.errors.get(), 1);
    assert_eq!(requests.bytes.get(), 12);
    assert_eq!(requests.seconds.count(), 2);
}

/// Catches two commands sharing one `command` label (their latencies would be
/// mixed), and a label that is not the command's name.
#[test]
fn each_command_has_its_own_label() {
    let object = ObjectId::new(kbf_meta::Epoch::new(1), 1);
    let commands = [
        Command::Tick(FarmTime::from_millis(1)),
        Command::AllocEpoch,
        Command::PutBlob {
            digest: kbf_segments::sha256(b"a"),
            location: kbf_meta::Location {
                store: kbf_meta::StoreId::CONFIGURED,
                object,
                offset: 0,
            },
        },
        Command::PutBlobs(Vec::new()),
        Command::PutAction {
            role: kbf_meta::Role::Daemon,
            action: kbf_segments::sha256(b"b"),
            record: kbf_meta::ActionRecord {
                result: kbf_segments::sha256(b"c"),
                closure: kbf_meta::Closure::new(),
            },
        },
        Command::Touch(Touch::default()),
        Command::ObjectUnreachable {
            object,
            reason: UnreachableReason::Missing,
        },
        Command::ObjectReachable {
            object,
            generation: Generation::new(1),
        },
        Command::Collect,
        Command::AddTier {
            server: kbf_meta::ServerId::new(1),
            tier: kbf_meta::TierId::new(1),
        },
        Command::PutStaged {
            tier: kbf_meta::TierId::new(1),
            objects: Vec::new(),
        },
        Command::SealStaged { object },
        Command::ObjectFlushed { object },
        Command::FlushConflict { object },
        Command::EvictObjects {
            objects: [object].into(),
        },
        Command::SetWriter(kbf_meta::NodeWriter::new(
            std::num::NonZeroU64::MIN,
            std::num::NonZeroU64::MIN,
        )),
        Command::AdoptSegment {
            object,
            entries: Vec::new(),
        },
        Command::RebuildProgress {
            after: String::new(),
        },
        Command::RebuildDone,
        Command::ReseatStaged {
            tier: kbf_meta::TierId::new(1),
            from: object,
            to: kbf_meta::StagedPut {
                object,
                end: 0,
                seal: true,
                blobs: Vec::new(),
            },
            stored: false,
        },
    ];
    let labels: Vec<&str> = commands
        .iter()
        .map(|c| COMMANDS[command_label(c)])
        .collect();
    assert_eq!(labels, COMMANDS);
}

/// Catches a family left out of the scrape, printed twice, or without the `# HELP`
/// and `# TYPE` lines a scraper reads its kind from; and a counter or gauge printed
/// with another family's value.
#[test]
fn every_family_is_printed_once_with_its_type() {
    let metrics = CacheMetrics::default();
    metrics.write.bytes.add(11);
    metrics.write_present.add(12);
    metrics.batch_update.bytes.add(13);
    metrics.batch_update_blobs.add(14);
    metrics.find_missing_digests.add(15);
    metrics.find_missing_missing.add(16);
    let _raised = metrics.batch_update.in_flight.raise();
    metrics.execute_dir_reads.add(17);
    metrics.execute_dir_cache_hits.add(23);
    metrics.execute_dir_cache_misses.add(24);
    metrics.execute_dir_cache_merged.add(25);
    metrics.execute_dir_cache_evictions.add(26);
    metrics.execute_dir_cache_present_checked.add(27);
    metrics.execute_dir_cache_bytes.set(28);
    metrics.execute_walk_dir_read.observe(29);
    metrics.execute_walk_dir_read.observe(30);
    metrics.sched_queued.set(18);
    metrics.sched_leases.set(19);
    let _presubmit = metrics.execute_presubmit_in_flight.raise();
    let (_tier, _tier2) = (
        metrics.tier_in_flight.raise(),
        metrics.tier_in_flight.raise(),
    );
    metrics.open_write_fds.set(21);
    metrics.write_pool_direct.add(22);
    metrics.write_pool_queue.add_one();
    metrics.write_pool_queue.add_one();
    metrics.write_pool_queue.add_one();
    let text = metrics.render();
    let types: Vec<&str> = lines(&text, "# TYPE ")
        .iter()
        .map(|l| l.trim_start_matches("# TYPE "))
        .collect();
    assert_eq!(
        types,
        [
            "kbf_bytestream_write_requests_total counter",
            "kbf_bytestream_write_errors_total counter",
            "kbf_bytestream_write_bytes_total counter",
            "kbf_bytestream_write_in_flight gauge",
            "kbf_bytestream_write_seconds histogram",
            "kbf_bytestream_write_present_total counter",
            "kbf_batch_update_requests_total counter",
            "kbf_batch_update_errors_total counter",
            "kbf_batch_update_bytes_total counter",
            "kbf_batch_update_in_flight gauge",
            "kbf_batch_update_seconds histogram",
            "kbf_batch_update_blobs_total counter",
            "kbf_find_missing_requests_total counter",
            "kbf_find_missing_errors_total counter",
            "kbf_find_missing_in_flight gauge",
            "kbf_find_missing_seconds histogram",
            "kbf_find_missing_digests_total counter",
            "kbf_find_missing_missing_total counter",
            "kbf_store_blobs_seconds histogram",
            "kbf_segment_put_seconds histogram",
            "kbf_segment_footer_read_seconds histogram",
            "kbf_segment_commit_seconds histogram",
            "kbf_segment_blobs histogram",
            "kbf_segment_bytes histogram",
            "kbf_object_get_seconds histogram",
            "kbf_meta_query_seconds histogram",
            "kbf_execute_presubmit_seconds histogram",
            "kbf_execute_walk_seconds histogram",
            "kbf_execute_walk_dirs_seconds histogram",
            "kbf_execute_walk_files_seconds histogram",
            "kbf_execute_walk_dir_read_seconds histogram",
            "kbf_execute_submit_seconds histogram",
            "kbf_sched_lock_wait_seconds histogram",
            "kbf_tier_wait_seconds histogram",
            "kbf_tier_call_seconds histogram",
            "kbf_execute_presubmit_in_flight gauge",
            "kbf_execute_dir_reads_total counter",
            "kbf_execute_walk_dirs_read_total counter",
            "kbf_execute_walk_dirs_skipped_total counter",
            "kbf_execute_walk_file_asks_total counter",
            "kbf_execute_walk_files_checked_total counter",
            "kbf_execute_dir_cache_hits_total counter",
            "kbf_execute_dir_cache_misses_total counter",
            "kbf_execute_dir_cache_merged_total counter",
            "kbf_execute_dir_cache_evictions_total counter",
            "kbf_execute_dir_cache_present_checked_total counter",
            "kbf_input_directories_cached_total counter",
            "kbf_input_directories_skipped_total counter",
            "kbf_input_directories_joined_total counter",
            "kbf_execute_dir_cache_bytes gauge",
            "kbf_sched_queued gauge",
            "kbf_sched_leases gauge",
            "kbf_estimator_shadow_total counter",
            "kbf_estimator_shadow_skipped_total counter",
            "kbf_prediction_made_total counter",
            "kbf_prediction_unset_total counter",
            "kbf_prediction_vs_run_total counter",
            "kbf_placement_cpu_limited_nodes gauge",
            "kbf_tier_in_flight gauge",
            "kbf_storage_open_write_fds gauge",
            "kbf_storage_write_pool_queue gauge",
            "kbf_storage_write_pool_direct_total counter",
            "kbf_meta_commit_seconds histogram",
            "kbf_execute_requests_total counter",
            "kbf_execute_errors_total counter",
            "kbf_execute_in_flight gauge",
            "kbf_front_open_executions gauge",
            "kbf_front_executes_started_total counter",
            "kbf_execute_queued gauge",
            "kbf_execute_prepare_seconds histogram",
            "kbf_execute_dispatch_seconds histogram",
            "kbf_execute_queued_seconds histogram",
            "kbf_execute_total_seconds histogram",
        ]
    );
    assert_eq!(lines(&text, "# HELP ").len(), types.len());
    for (name, value) in [
        ("kbf_bytestream_write_bytes_total", 11),
        ("kbf_bytestream_write_present_total", 12),
        ("kbf_batch_update_bytes_total", 13),
        ("kbf_batch_update_blobs_total", 14),
        ("kbf_find_missing_digests_total", 15),
        ("kbf_find_missing_missing_total", 16),
        ("kbf_batch_update_in_flight", 1),
        ("kbf_bytestream_write_in_flight", 0),
        ("kbf_execute_dir_reads_total", 17),
        ("kbf_execute_dir_cache_hits_total", 23),
        ("kbf_execute_dir_cache_misses_total", 24),
        ("kbf_execute_dir_cache_merged_total", 25),
        ("kbf_execute_dir_cache_evictions_total", 26),
        ("kbf_execute_dir_cache_present_checked_total", 27),
        ("kbf_execute_dir_cache_bytes", 28),
        ("kbf_execute_walk_dir_read_seconds_count", 2),
        ("kbf_sched_queued", 18),
        ("kbf_sched_leases", 19),
        ("kbf_execute_presubmit_in_flight", 1),
        ("kbf_tier_in_flight", 2),
        ("kbf_storage_open_write_fds", 21),
        ("kbf_storage_write_pool_queue", 3),
        ("kbf_storage_write_pool_direct_total", 22),
    ] {
        assert_eq!(
            lines(&text, &format!("{name} ")),
            [format!("{name} {value}")]
        );
    }
    let commits = lines(&text, "kbf_meta_commit_seconds_count");
    let expected: Vec<String> = COMMANDS
        .iter()
        .map(|c| format!("kbf_meta_commit_seconds_count{{command=\"{c}\"}} 0"))
        .collect();
    assert_eq!(commits, expected);
}

/// Catches an Execute guard that does not give back `in_flight` and `queued` however
/// the stream ends (a caller that leaves while the operation is queued would leave both
/// raised for ever), a stream counted as answered without its done operation, a
/// queued time observed more than once or when the stream never left `QUEUED`, and a
/// WaitExecution stream that feeds anything but `in_flight`.
#[test]
fn an_execute_is_recorded_however_its_stream_ends() {
    let metrics = Arc::new(ExecuteMetrics::default());

    // Answered: dispatched, queued, executing, done.
    let mut answered = metrics.execute();
    assert_eq!(metrics.in_flight.get(), 1);
    answered.dispatching();
    answered.dispatched(true);
    assert_eq!(metrics.queued.get(), 1);
    answered.update(true, false);
    assert_eq!(metrics.queued.get(), 1, "still QUEUED");
    answered.update(false, false);
    assert_eq!(metrics.queued.get(), 0);
    answered.update(false, true);
    drop(answered);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (1, 0));

    // Left by its caller while queued.
    let mut left = metrics.execute();
    left.dispatching();
    left.dispatched(true);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (1, 1));
    drop(left);
    assert_eq!((metrics.in_flight.get(), metrics.queued.get()), (0, 0));
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (2, 1));

    // Refused by the scheduler: its call was timed, and it was never queued.
    let mut refused = metrics.execute();
    refused.dispatching();
    refused.dispatched(false);
    assert_eq!(metrics.queued.get(), 0);
    drop(refused);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (3, 2));

    // A WaitExecution stream is open, and nothing else.
    let waiting = metrics.wait();
    assert_eq!(metrics.in_flight.get(), 1);
    drop(waiting);
    assert_eq!(metrics.in_flight.get(), 0);
    assert_eq!((metrics.requests.get(), metrics.errors.get()), (3, 2));

    assert_eq!(metrics.prepare.count(), 3);
    assert_eq!(metrics.dispatch.count(), 3);
    assert_eq!(metrics.queued_seconds.count(), 1);
    assert_eq!(metrics.total.count(), 3);
}

/// Catches `streaming` raising `open_executions` once per call rather than once per
/// guard (a stream that asked twice would leave the gauge raised for ever after its one
/// lowering), and a guard that never streamed lowering it.
#[test]
fn open_executions_is_raised_once_per_guard_and_only_by_a_streaming_one() {
    let metrics = Arc::new(ExecuteMetrics::default());
    let mut streamed = metrics.execute();
    let plain = metrics.wait();
    assert_eq!(metrics.open_executions.get(), 0, "not before streaming()");
    streamed.streaming();
    streamed.streaming();
    assert_eq!(metrics.open_executions.get(), 1);
    drop(plain);
    assert_eq!(
        metrics.open_executions.get(),
        1,
        "a guard that never streamed"
    );
    drop(streamed);
    assert_eq!(metrics.open_executions.get(), 0);
}

/// Catches: a bucket or level printed under another's label (a dashboard would read
/// `would_oom` as `covered`), a pair missing from the scrape, and a count of one pair
/// leaking into its neighbour.
#[test]
fn the_shadow_totals_print_each_level_and_bucket_under_its_own_label() {
    let m = CacheMetrics::default();
    m.estimator_shadow
        .set(ShadowLevel::L2, ShadowBucket::WouldOom, 7);
    m.estimator_shadow
        .set(ShadowLevel::None, ShadowBucket::Cold, 3);
    let text = m.render();
    let rows = lines(&text, "kbf_estimator_shadow_total{");
    assert_eq!(rows.len(), 12);
    assert!(rows.contains(&r#"kbf_estimator_shadow_total{level="l2",bucket="would_oom"} 7"#));
    assert!(rows.contains(&r#"kbf_estimator_shadow_total{level="none",bucket="cold"} 3"#));
    assert!(rows.contains(&r#"kbf_estimator_shadow_total{level="l1",bucket="covered"} 0"#));
    assert!(rows.contains(&r#"kbf_estimator_shadow_total{level="l2",bucket="censored"} 0"#));
    m.estimator_shadow.set_skipped(1, 4);
    m.estimator_shadow.set_skipped(99, 1);
    assert_eq!(m.estimator_shadow.skipped(1), 4);
    assert_eq!(m.estimator_shadow.skipped(99), 0);
    let text = m.render();
    let skips = lines(&text, "kbf_estimator_shadow_skipped_total{");
    assert_eq!(skips.len(), SHADOW_SKIPS.len());
    assert!(skips.contains(&r#"kbf_estimator_shadow_skipped_total{reason="no_os"} 4"#));
    assert!(skips.contains(&r#"kbf_estimator_shadow_skipped_total{reason="kind"} 0"#));
}

/// Catches: `made` printed under another series, a reason or an axis and bucket printed under
/// another's label (a dashboard would read `cold` as `missing`, or `under` as `over`), a
/// series missing from the scrape, a count of one pair leaking into its neighbour, and an
/// index past the end that is not ignored.
#[test]
fn the_prediction_totals_print_each_series_under_its_own_label() {
    let m = CacheMetrics::default();
    m.prediction.set_made(11);
    for (reason, label) in PREDICTION_UNSET.iter().enumerate() {
        m.prediction.set_unset(reason, 100 + reason as u64);
        assert_eq!(m.prediction.unset(reason), 100 + reason as u64, "{label}");
    }
    for (axis, bucket) in [(0, 2), (1, 0), (2, 1)] {
        m.prediction
            .set_vs_run(axis, bucket, 20 + (axis * 3 + bucket) as u64);
    }
    m.prediction.set_unset(99, 1);
    m.prediction.set_vs_run(3, 0, 1);
    m.prediction.set_vs_run(0, 3, 1);
    m.prediction.set_cpu_limited_nodes(4);
    assert_eq!(m.prediction.made(), 11);
    assert_eq!(m.prediction.unset(99), 0);
    assert_eq!(m.prediction.vs_run(3, 0), 0);
    assert_eq!(m.prediction.vs_run(0, 3), 0);
    assert_eq!(m.prediction.cpu_limited_nodes(), 4);

    let text = m.render();
    assert_eq!(
        lines(&text, "kbf_prediction_made_total "),
        ["kbf_prediction_made_total 11"]
    );
    let unset = lines(&text, "kbf_prediction_unset_total{");
    assert_eq!(unset.len(), 8);
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="missing"} 100"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="stale"} 101"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="cold"} 102"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="corrupt"} 103"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="over_cap"} 104"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="kind"} 105"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="no_os"} 106"#));
    assert!(unset.contains(&r#"kbf_prediction_unset_total{reason="no_subject"} 107"#));
    let vs_run = lines(&text, "kbf_prediction_vs_run_total{");
    assert_eq!(vs_run.len(), 9);
    assert!(vs_run.contains(&r#"kbf_prediction_vs_run_total{axis="cpu",bucket="over"} 22"#));
    assert!(vs_run.contains(&r#"kbf_prediction_vs_run_total{axis="mem",bucket="under"} 23"#));
    assert!(vs_run.contains(&r#"kbf_prediction_vs_run_total{axis="life",bucket="fit"} 27"#));
    assert!(vs_run.contains(&r#"kbf_prediction_vs_run_total{axis="cpu",bucket="under"} 0"#));
    assert!(vs_run.contains(&r#"kbf_prediction_vs_run_total{axis="mem",bucket="over"} 0"#));
    assert_eq!(
        lines(&text, "kbf_placement_cpu_limited_nodes "),
        ["kbf_placement_cpu_limited_nodes 4"]
    );
}

/// Asserts that each cause of an Execute's end bumps its own series and no other: an
/// error status (also when the connection is gone), a stream dropped on a connection that
/// is still there, one dropped on a lost connection, and one dropped with no connection
/// to ask; that an answered call bumps none; and that the causes and the answers add up
/// to the requests.
///
/// Catches: two causes swapped, a status read as a drop, a drop with no connection read
/// as an abort, and an answer counted as an error: the abort and dropped arms of
/// `OpenExecute::cause` swapped, `is_gone` tested the wrong way round, `failed` ignored.
#[test]
fn each_end_of_an_execute_has_its_own_cause() {
    use kbf_tls::Liveness;

    let metrics = Arc::new(ExecuteMetrics::default());
    let by = |m: &ExecuteMetrics| {
        [
            ErrorCause::Status,
            ErrorCause::ClientAbort,
            ErrorCause::Dropped,
        ]
        .map(|c| m.errors.of(c))
    };

    let live = Liveness::default();
    let mut aborted = metrics.execute();
    aborted.connection(Some(live.clone()));
    drop(aborted);
    assert_eq!(by(&metrics), [0, 1, 0], "a reset on a live connection");

    let mut failed = metrics.execute();
    failed.connection(Some(live.clone()));
    failed.fail();
    drop(failed);
    assert_eq!(by(&metrics), [1, 1, 0], "an error status");

    let mut answered = metrics.execute();
    answered.connection(Some(live.clone()));
    answered.ok();
    drop(answered);
    assert_eq!(by(&metrics), [1, 1, 0], "an answer is no error");

    let mut unasked = metrics.execute();
    unasked.connection(None);
    drop(unasked);
    assert_eq!(by(&metrics), [1, 1, 1], "no connection to ask");

    let mut lost = metrics.execute();
    lost.connection(Some(live.clone()));
    live.mark_gone();
    drop(lost);
    assert_eq!(by(&metrics), [1, 1, 2], "a lost connection");

    let mut failed_and_lost = metrics.execute();
    failed_and_lost.connection(Some(live));
    failed_and_lost.fail();
    drop(failed_and_lost);
    assert_eq!(
        by(&metrics),
        [2, 1, 2],
        "a status wins over a lost connection"
    );

    assert_eq!(metrics.errors.get(), 5);
    assert_eq!(
        metrics.requests.get(),
        metrics.errors.get() + 1,
        "one answer"
    );
}

/// Asserts that the exposition has one series per cause, in a fixed order, at 0 before any
/// error, with the labels spelled as the dashboards and the gate read them.
///
/// Catches: a cause series absent until first bumped (an absent series would read as a
/// bug to the gate), a renamed label value, an unlabeled series left beside them, and
/// a cause series rendered only when non-zero.
#[test]
fn every_cause_is_rendered_from_the_start() {
    let metrics = Arc::new(ExecuteMetrics::default());
    let mut out = Families(String::new());
    metrics.render(&mut out);
    assert_eq!(
        lines(&out.0, "kbf_execute_errors_total"),
        [
            r#"kbf_execute_errors_total{cause="client_abort"} 0"#,
            r#"kbf_execute_errors_total{cause="dropped"} 0"#,
            r#"kbf_execute_errors_total{cause="status"} 0"#,
        ]
    );
    drop(metrics.execute());
    let mut out = Families(String::new());
    metrics.render(&mut out);
    assert_eq!(
        lines(&out.0, "kbf_execute_errors_total{"),
        [
            r#"kbf_execute_errors_total{cause="client_abort"} 0"#,
            r#"kbf_execute_errors_total{cause="dropped"} 1"#,
            r#"kbf_execute_errors_total{cause="status"} 0"#,
        ]
    );
}

fn whole_summary(
    held: usize,
    waiting: &[(kbf_sched::WholeReason, usize)],
) -> kbf_sched::WholeSummary {
    kbf_sched::WholeSummary {
        held,
        marks: 2,
        whole: held + 2,
        draining: 1,
        ceiling: 5,
        max_whole_node: 6,
        capable_hosts: 7,
        keep_for_ordinary: 1,
        max_draining: 2,
        waiting: waiting.iter().copied().collect(),
    }
}

/// Catches the whole-node gauges rendered with the option off (a scrape must be what it was),
/// a gauge not set from the summary (each of held, marks, draining, ceiling), a reason
/// without its series or with another's count, and a count that stays after the requests
/// that waited for the reason have gone.
#[test]
fn the_whole_node_gauges_are_rendered_only_when_set_and_follow_the_summary() {
    use kbf_sched::{Blocked, WholeReason};
    let metrics = CacheMetrics::default();
    assert!(!metrics.render().contains("kbf_sched_whole"), "off");
    metrics.sched_whole.set(Some(&whole_summary(
        3,
        &[
            (WholeReason::Holding, 4),
            (WholeReason::Cap(Blocked::MaxDraining), 5),
            (WholeReason::Unservable, 6),
        ],
    )));
    let text = metrics.render();
    for family in [
        "kbf_sched_whole_held",
        "kbf_sched_whole_marks",
        "kbf_sched_whole_draining",
        "kbf_sched_whole_ceiling",
        "kbf_sched_whole_waiting",
    ] {
        assert!(text.contains(&format!("# TYPE {family} gauge")), "{family}");
    }
    let series = lines(&text, "kbf_sched_whole_");
    assert_eq!(
        series,
        [
            "kbf_sched_whole_held 3",
            "kbf_sched_whole_marks 2",
            "kbf_sched_whole_draining 1",
            "kbf_sched_whole_ceiling 5",
            r#"kbf_sched_whole_waiting{reason="early"} 0"#,
            r#"kbf_sched_whole_waiting{reason="holding"} 4"#,
            r#"kbf_sched_whole_waiting{reason="barred"} 0"#,
            r#"kbf_sched_whole_waiting{reason="cap_ceiling"} 0"#,
            r#"kbf_sched_whole_waiting{reason="cap_keep_for_ordinary"} 0"#,
            r#"kbf_sched_whole_waiting{reason="cap_max_draining"} 5"#,
            r#"kbf_sched_whole_waiting{reason="cap_per_consumer"} 0"#,
            r#"kbf_sched_whole_waiting{reason="last_capable"} 0"#,
            r#"kbf_sched_whole_waiting{reason="queued"} 0"#,
            r#"kbf_sched_whole_waiting{reason="unservable"} 6"#,
        ]
    );
    metrics.sched_whole.set(Some(&whole_summary(0, &[])));
    assert_eq!(metrics.sched_whole.waiting(WholeReason::Holding), 0);
    assert_eq!(metrics.sched_whole.held(), 0);
    metrics.sched_whole.set(None);
    assert!(!metrics.render().contains("kbf_sched_whole"), "off again");
}
