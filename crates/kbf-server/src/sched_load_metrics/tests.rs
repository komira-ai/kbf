//! The `kbf_sched_load_*` families as a scrape writes them (`crate::metrics::render_farm`, the
//! one call of `GET /metrics`), over the farm.

use std::sync::Arc;

use kbf_sched::LoadCounters;

use super::{COUNTS, render_counters};
use crate::farm::Farm;

fn farm() -> Farm {
    Farm::new(
        Arc::new(kbf_front::Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
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

/// Asserts that a farm with no reading writes the scheduler's counters at 0: a scheduler that
/// has counted nothing is a fact.
#[tokio::test(start_paused = true)]
async fn a_farm_with_no_readings_writes_zero_counters() {
    let farm = farm();
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
}
