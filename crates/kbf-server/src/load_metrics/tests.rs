//! The `kbf_sched_load_*` and `kbf_node_load_*` families as a scrape writes them
//! (`crate::metrics::render_farm`, the one call of `GET /metrics`), over a farm on the core
//! under test: the farm's own, and the js core in a build with `farm-core-js`. The farm's clock
//! is tokio's, and these tests stop and advance it; none reads a real one.

use std::sync::Arc;
use std::time::Duration;

use kbf_proto::worker::{LeaseClock, LeaseId as ProtoLease, LeaseMinute, NodeLoad, ServerMessage};
use kbf_sched::{DaemonInstance, LoadCounters};
use kbf_types::{Resources, WorkerId};

use super::{COUNTS, render_counters, render_nodes};
use crate::farm::{Farm, Reported, StreamId};

const GIB: u64 = 1 << 30;
const SECOND: u64 = 1_000_000;

fn farm() -> Farm {
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
}

/// `name` registered by daemon process `instance` on a new stream, on a node of 4 CPUs and
/// 64 GiB, kept open for the rest of the test.
fn register(farm: &Farm, name: &str, instance: &str) -> StreamId {
    let report = [("arch", "x86_64"), ("os", "linux")];
    let caps = kbf_caps::NodeCaps::from_report(report).expect("caps");
    let reported = Reported::serving(
        Resources::new(4_000, 64 * GIB),
        caps.with_drivers(["container"]),
    );
    let (outbound, inbox) = tokio::sync::mpsc::unbounded_channel();
    std::mem::forget(inbox);
    farm.register(
        &WorkerId::new(name),
        DaemonInstance::new(instance),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    )
}

fn lease(seq: u64) -> ProtoLease {
    ProtoLease { term: 1, seq }
}

fn clock(seq: u64, started_mono_usec: u64) -> LeaseClock {
    LeaseClock {
        lease: Some(lease(seq)),
        started_mono_usec,
    }
}

fn minute(seq: u64, minute: u32) -> LeaseMinute {
    LeaseMinute {
        lease: Some(lease(seq)),
        minute,
        cpu_mean_millis: 100,
        cpu_peak_window_millis: 200,
        ws_max_bytes: 1 << 20,
    }
}

/// A reading `seq`, taken at `mono` microseconds, with `usage` microseconds of CPU used in all.
fn reading(seq: u64, mono: u64, usage: u64) -> NodeLoad {
    NodeLoad {
        seq,
        mono_usec: mono,
        cpu_usage_usec: usage,
        ..NodeLoad::default()
    }
}

/// A heartbeat carrying `load`, as the worker session handles one: the accepted reading goes to
/// the scheduler with the heartbeat, and a refused one leaves it with none.
async fn beat(farm: &Farm, worker: &WorkerId, stream: StreamId, seq: u64, load: &NodeLoad) {
    let fed = farm.note_node_load(worker, stream, Some(load));
    assert!(
        farm.heartbeat_with_load(worker, stream, seq, Vec::new(), fed)
            .await
    );
}

/// What a scrape of `farm` says.
fn scrape(farm: &Farm) -> String {
    let mut text = String::new();
    crate::metrics::render_farm(&mut text, farm);
    text
}

/// The value of the sample `series` (a name, with its labels if it has any) in `text`.
fn sample(text: &str, series: &str) -> Option<String> {
    let prefix = format!("{series} ");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::to_owned)
}

/// Asserts that each sample of `expected` is in `text` with its value.
#[track_caller]
fn assert_samples(text: &str, expected: &[(&str, &str)]) {
    for (series, value) in expected {
        assert_eq!(
            sample(text, series).as_deref(),
            Some(*value),
            "{series}\n{text}"
        );
    }
}

/// Asserts that no line of `text` is a sample or a header of the family `name`, labelled or not.
#[track_caller]
fn assert_no_family(text: &str, name: &str) {
    let written = text.lines().any(|line| {
        let line = line
            .strip_prefix("# HELP ")
            .or_else(|| line.strip_prefix("# TYPE "))
            .unwrap_or(line);
        line.split([' ', '{']).next() == Some(name)
    });
    assert!(!written, "{name} is written\n{text}");
}

/// Asserts that every field of the scheduler's counters has a family of its own that samples
/// it: the fields are given different values, so that two families reading one field, or a
/// family reading another's, is told apart. The 24th field is the one gauge.
#[test]
fn each_scheduler_counter_samples_its_own_field() {
    let counters = LoadCounters {
        admitted: 1,
        admitted_fallback: 2,
        admitted_cold: 3,
        curve_flat: 4,
        profile_cpu: 5,
        profile_memory: 6,
        misfit_cpu_minutes: 7,
        misfit_memory_minutes: 8,
        demotions_cpu: 9,
        demotions_memory: 10,
        blocked: 11,
        cold_deferred: 12,
        reservations: 13,
        readings_accepted: 14,
        readings_dropped: 15,
        floor_windows: 16,
        floor_binds: 17,
        score_evaluations: 18,
        scored: 19,
        chose_slack: 20,
        chose_distrusted: 21,
        chose_concentrated: 22,
        chose_by_tie_break: 23,
        fallback_cpu_limited: 24,
    };
    let mut text = String::new();
    render_counters(&mut text, &counters);
    assert_samples(
        &text,
        &[
            ("kbf_sched_load_admitted_total", "1"),
            ("kbf_sched_load_admitted_fallback_total", "2"),
            ("kbf_sched_load_admitted_cold_total", "3"),
            ("kbf_sched_load_curve_flat_total", "4"),
            ("kbf_sched_load_profile_cpu_total", "5"),
            ("kbf_sched_load_profile_memory_total", "6"),
            ("kbf_sched_load_misfit_cpu_minutes_total", "7"),
            ("kbf_sched_load_misfit_memory_minutes_total", "8"),
            ("kbf_sched_load_demotions_cpu_total", "9"),
            ("kbf_sched_load_demotions_memory_total", "10"),
            ("kbf_sched_load_blocked_total", "11"),
            ("kbf_sched_load_cold_deferred_total", "12"),
            ("kbf_sched_load_reservations_total", "13"),
            ("kbf_sched_load_readings_accepted_total", "14"),
            ("kbf_sched_load_readings_dropped_total", "15"),
            ("kbf_sched_load_floor_windows_total", "16"),
            ("kbf_sched_load_floor_binds_total", "17"),
            ("kbf_sched_load_score_evaluations_total", "18"),
            ("kbf_sched_load_scored_total", "19"),
            ("kbf_sched_load_chose_slack_total", "20"),
            ("kbf_sched_load_chose_distrusted_total", "21"),
            ("kbf_sched_load_chose_concentrated_total", "22"),
            ("kbf_sched_load_chose_by_tie_break_total", "23"),
            ("kbf_sched_load_fallback_cpu_limited_nodes", "24"),
        ],
    );
    assert!(
        text.contains("# TYPE kbf_sched_load_fallback_cpu_limited_nodes gauge\n"),
        "{text}"
    );
    assert_eq!(
        COUNTS.len() + 1,
        text.lines().filter(|l| l.starts_with("# TYPE ")).count()
    );
    for (name, ..) in COUNTS {
        assert!(name.ends_with("_total"), "{name}");
        assert!(text.contains(&format!("# TYPE {name} counter\n")), "{text}");
    }
}

/// Asserts that a farm with no reading writes the scheduler's counters at 0 (a scheduler that
/// has counted nothing is a fact) and no node family at all, and that no node has no node family
/// whoever calls.
#[tokio::test(start_paused = true)]
async fn a_farm_with_no_readings_writes_zero_counters_and_no_node_family() {
    let farm = farm();
    register(&farm, "a", "proc-a");
    let text = scrape(&farm);
    assert_samples(
        &text,
        &[
            ("kbf_sched_load_readings_accepted_total", "0"),
            ("kbf_sched_load_readings_dropped_total", "0"),
            ("kbf_sched_load_misfit_cpu_minutes_total", "0"),
            ("kbf_sched_load_misfit_memory_minutes_total", "0"),
        ],
    );
    assert!(!text.contains("kbf_node_load_"), "{text}");
    let mut empty = String::new();
    render_nodes(&mut empty, &[]);
    assert_eq!(empty, "");
}

/// Asserts that after accepted readings a scrape shows the scheduler's accepted count and, for
/// each node with a reading, one sample of each gauge and of each list counter with the values of
/// its view (`GET /v1/nodes/{id}`): the measured CPU of two windows, the working set, the active
/// file pages, the admission working set, `stale` and `actions_cpu_limited` as 0 or 1, the
/// telemetry the daemon read, what the vet kept and what it dropped by reason. A node's own
/// values are not another's, and a telemetry value the daemon did not read has no sample.
#[tokio::test(start_paused = true)]
async fn a_scrape_after_accepted_readings_shows_the_counters_and_each_nodes_load() {
    let farm = farm();
    let (a, b) = (WorkerId::new("a"), WorkerId::new("b"));
    let stream_a = register(&farm, "a", "proc-a");
    let stream_b = register(&farm, "b", "proc-b");

    // Of `a`'s first reading, the clocks, minutes and `covered` have each some records that
    // break a rule or repeat one: 1 clock and 1 minute are kept.
    let broken = LeaseMinute {
        cpu_mean_millis: 300,
        ..minute(7, 1)
    };
    let first = NodeLoad {
        covered: (0..100).map(lease).collect(),
        lease_clocks: vec![
            clock(1, SECOND),
            clock(1, 2 * SECOND),
            clock(2, 11 * SECOND),
        ],
        minutes: vec![minute(7, 0), broken, minute(7, 0)],
        memory_working_set_bytes: 1_000,
        memory_active_file_bytes: 200,
        cpu_throttled_usec: Some(7),
        cpu_pressure_some_total_usec: Some(12_345),
        ..reading(1, 10 * SECOND, 0)
    };
    beat(&farm, &a, stream_a, 1, &first).await;
    // Two seconds of CPU in the second after it, and 2 minutes more.
    let second = NodeLoad {
        minutes: vec![minute(7, 1), minute(7, 2)],
        ..first_state(2, 11 * SECOND, 2 * SECOND)
    };
    beat(&farm, &a, stream_a, 2, &second).await;

    // `b` has one reading, so no window, and a CPU limit.
    let only = NodeLoad {
        memory_working_set_bytes: 5_000,
        actions_cpu_limited: true,
        ..reading(1, 10 * SECOND, 0)
    };
    beat(&farm, &b, stream_b, 1, &only).await;

    let text = scrape(&farm);
    assert_samples(
        &text,
        &[
            ("kbf_sched_load_readings_accepted_total", "3"),
            ("kbf_sched_load_readings_dropped_total", "0"),
            ("kbf_node_load_measured_cpu_millicores{node=\"a\"}", "2000"),
            ("kbf_node_load_memory_working_set_bytes{node=\"a\"}", "1000"),
            ("kbf_node_load_memory_working_set_bytes{node=\"b\"}", "5000"),
            ("kbf_node_load_memory_active_file_bytes{node=\"a\"}", "200"),
            ("kbf_node_load_memory_active_file_bytes{node=\"b\"}", "0"),
            (
                "kbf_node_load_admission_working_set_bytes{node=\"a\"}",
                "1000",
            ),
            (
                "kbf_node_load_admission_working_set_bytes{node=\"b\"}",
                "5000",
            ),
            ("kbf_node_load_stale{node=\"a\"}", "0"),
            ("kbf_node_load_stale{node=\"b\"}", "0"),
            ("kbf_node_load_actions_cpu_limited{node=\"a\"}", "0"),
            ("kbf_node_load_actions_cpu_limited{node=\"b\"}", "1"),
            ("kbf_node_load_cpu_throttled_usec_total{node=\"a\"}", "7"),
            (
                "kbf_node_load_cpu_pressure_some_usec_total{node=\"a\"}",
                "12345",
            ),
            ("kbf_node_load_clocks_total{node=\"a\"}", "1"),
            ("kbf_node_load_clocks_total{node=\"b\"}", "0"),
            ("kbf_node_load_minutes_total{node=\"a\"}", "3"),
            ("kbf_node_load_minutes_total{node=\"b\"}", "0"),
            ("kbf_node_load_refused_minutes_total{node=\"a\"}", "0"),
            (
                "kbf_node_load_dropped_total{node=\"a\",reason=\"invalid\"}",
                "2",
            ),
            (
                "kbf_node_load_dropped_total{node=\"a\",reason=\"duplicate\"}",
                "2",
            ),
            (
                "kbf_node_load_dropped_total{node=\"a\",reason=\"over_cap\"}",
                "4",
            ),
            (
                "kbf_node_load_dropped_total{node=\"b\",reason=\"invalid\"}",
                "0",
            ),
        ],
    );
    // One series per node, none for what a node did not say: `b` has no window and read no
    // telemetry.
    for series in [
        "kbf_node_load_measured_cpu_millicores{node=\"b\"}",
        "kbf_node_load_cpu_throttled_usec_total{node=\"b\"}",
        "kbf_node_load_cpu_pressure_some_usec_total{node=\"b\"}",
    ] {
        assert_eq!(sample(&text, series), None, "{series}\n{text}");
    }
    assert_eq!(
        text.lines()
            .filter(|l| l.starts_with("kbf_node_load_stale{"))
            .count(),
        2,
        "{text}"
    );
}

/// `reading(seq, mono, usage)` with the memory figures and the telemetry that `a` reports.
fn first_state(seq: u64, mono: u64, usage: u64) -> NodeLoad {
    NodeLoad {
        memory_working_set_bytes: 1_000,
        memory_active_file_bytes: 200,
        cpu_throttled_usec: Some(7),
        cpu_pressure_some_total_usec: Some(12_345),
        ..reading(seq, mono, usage)
    }
}

/// Asserts that a measured CPU the view leaves out is left out of the scrape and not written as
/// 0: a node with one reading has no window, and a node whose readings went stale has none; both
/// still have their other gauges (`kbf_node_load_stale` says 1 for the second). The family has no
/// header while no node has a sample of it, and a second reading gives it its sample.
#[tokio::test(start_paused = true)]
async fn an_absent_measured_cpu_is_not_exported() {
    let farm = farm();
    let a = WorkerId::new("a");
    let stream = register(&farm, "a", "proc-a");
    // Only the farm's own clock and the view are read here, so no heartbeat is sent: the
    // readings are noted, as a heartbeat's are, and the clock is tokio's, which only the test
    // advances.
    farm.note_node_load(&a, stream, Some(&reading(1, SECOND, 0)));
    let text = scrape(&farm);
    assert_no_family(&text, "kbf_node_load_measured_cpu_millicores");
    assert_samples(
        &text,
        &[
            ("kbf_node_load_memory_working_set_bytes{node=\"a\"}", "0"),
            ("kbf_node_load_stale{node=\"a\"}", "0"),
        ],
    );

    // Two readings a second apart: a window, and a CPU rate of 500.
    farm.note_node_load(&a, stream, Some(&reading(2, 2 * SECOND, SECOND / 2)));
    assert_samples(
        &scrape(&farm),
        &[("kbf_node_load_measured_cpu_millicores{node=\"a\"}", "500")],
    );

    // No heartbeat for more than three windows: stale, and no measured CPU.
    tokio::time::advance(Duration::from_secs(3)).await;
    let text = scrape(&farm);
    assert_samples(&text, &[("kbf_node_load_stale{node=\"a\"}", "0")]);
    assert!(sample(&text, "kbf_node_load_measured_cpu_millicores{node=\"a\"}").is_some());
    tokio::time::advance(Duration::from_millis(1)).await;
    let text = scrape(&farm);
    assert_samples(
        &text,
        &[
            ("kbf_node_load_stale{node=\"a\"}", "1"),
            ("kbf_node_load_memory_working_set_bytes{node=\"a\"}", "0"),
        ],
    );
    assert_no_family(&text, "kbf_node_load_measured_cpu_millicores");
}

/// Asserts that readings the accept rule refuses (a repeat of the last seq, an older one, a clock
/// that went back) count the minutes they carried in `kbf_node_load_refused_minutes_total` and
/// nothing else: the scheduler's accepted count and the node's gauges and kept counters do not
/// move, and the scheduler is not handed them (so `readings_dropped` stays 0).
#[tokio::test(start_paused = true)]
async fn a_refused_reading_increments_the_refused_counters() {
    let farm = farm();
    let a = WorkerId::new("a");
    let stream = register(&farm, "a", "proc-a");
    let with_minutes = |seq, mono, n| NodeLoad {
        minutes: vec![minute(7, 3); n],
        ..reading(seq, mono, 0)
    };
    beat(&farm, &a, stream, 1, &with_minutes(5, 10 * SECOND, 1)).await;
    let before = scrape(&farm);
    assert_samples(
        &before,
        &[
            ("kbf_sched_load_readings_accepted_total", "1"),
            ("kbf_node_load_refused_minutes_total{node=\"a\"}", "0"),
            ("kbf_node_load_minutes_total{node=\"a\"}", "1"),
        ],
    );

    beat(&farm, &a, stream, 2, &with_minutes(5, 11 * SECOND, 4)).await;
    beat(&farm, &a, stream, 3, &with_minutes(4, 11 * SECOND, 2)).await;
    beat(&farm, &a, stream, 4, &with_minutes(6, 9 * SECOND, 3)).await;
    assert_samples(
        &scrape(&farm),
        &[
            ("kbf_node_load_refused_minutes_total{node=\"a\"}", "9"),
            ("kbf_node_load_minutes_total{node=\"a\"}", "1"),
            ("kbf_sched_load_readings_accepted_total", "1"),
            ("kbf_sched_load_readings_dropped_total", "0"),
        ],
    );
}

/// Asserts that a reading the server accepted and the scheduler then drops (here, the same
/// accepted reading handed to it twice: its `seq` is not above the last) is counted in the
/// scheduler's `readings_dropped` and not in the node's refused counters, so the two are told
/// apart: the first is the scheduler refusing what the server believed, the second the server
/// refusing a heartbeat's reading.
#[tokio::test(start_paused = true)]
async fn a_reading_the_scheduler_drops_is_counted_apart_from_one_the_server_refuses() {
    let farm = farm();
    let a = WorkerId::new("a");
    let stream = register(&farm, "a", "proc-a");
    let fed = farm
        .note_node_load(&a, stream, Some(&reading(1, SECOND, 0)))
        .expect("accepted");
    for seq in [1, 2] {
        assert!(
            farm.heartbeat_with_load(&a, stream, seq, Vec::new(), Some(fed.clone()))
                .await
        );
    }
    assert_samples(
        &scrape(&farm),
        &[
            ("kbf_sched_load_readings_accepted_total", "1"),
            ("kbf_sched_load_readings_dropped_total", "1"),
            ("kbf_node_load_refused_minutes_total{node=\"a\"}", "0"),
        ],
    );
}
