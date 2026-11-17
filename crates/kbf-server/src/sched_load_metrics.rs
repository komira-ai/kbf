//! `kbf_sched_load_*` on `GET /metrics`: what the scheduler has counted of the node-load
//! readings it was handed and of placement by load ([`crate::farm::Farm::load_counters`]).
//!
//! Nothing hands the scheduler a reading in this release, so the readings counters stay at 0;
//! the placement counters are written as always. They are written on every scrape, 0 included:
//! a scheduler that has counted nothing is a fact.

use std::fmt::Write as _;

use kbf_sched::LoadCounters;

/// One scheduler family: its name, its `# HELP` text, and the count it samples.
type Count = (&'static str, &'static str, fn(&LoadCounters) -> u64);

/// The scheduler's counters, in the order they are written.
const COUNTS: [Count; 23] = [
    (
        "kbf_sched_load_readings_accepted_total",
        "Node-load readings the scheduler accepted",
        |c| c.readings_accepted,
    ),
    (
        "kbf_sched_load_readings_dropped_total",
        "Node-load readings the scheduler dropped: out of order, a counter or the clock going back, or implausible",
        |c| c.readings_dropped,
    ),
    (
        "kbf_sched_load_floor_windows_total",
        "Accepted readings that gave a window",
        |c| c.floor_windows,
    ),
    (
        "kbf_sched_load_floor_binds_total",
        "Windows in which the floor of the covered leases' predictions was above the measurement",
        |c| c.floor_binds,
    ),
    (
        "kbf_sched_load_misfit_cpu_minutes_total",
        "Lease-minutes whose mean cores were over twice the charge (and 250 millicores over it)",
        |c| c.misfit_cpu_minutes,
    ),
    (
        "kbf_sched_load_misfit_memory_minutes_total",
        "Lease-minutes whose working set was above the curve's remaining maximum",
        |c| c.misfit_memory_minutes,
    ),
    (
        "kbf_sched_load_demotions_cpu_total",
        "Times the shape guard charged a key's CPU flat for its next runs",
        |c| c.demotions_cpu,
    ),
    (
        "kbf_sched_load_demotions_memory_total",
        "Times the shape guard charged a key's memory flat for its next runs",
        |c| c.demotions_memory,
    ),
    (
        "kbf_sched_load_admitted_total",
        "Leases the load predicate admitted (--placement-load on)",
        |c| c.admitted,
    ),
    (
        "kbf_sched_load_admitted_fallback_total",
        "Of those, on a node charged by prediction alone",
        |c| c.admitted_fallback,
    ),
    (
        "kbf_sched_load_admitted_cold_total",
        "Of those, cold ones (a key with no usable estimate)",
        |c| c.admitted_cold,
    ),
    (
        "kbf_sched_load_curve_flat_total",
        "Of those, whose key has a curve that was not used: charged flat in CPU and memory",
        |c| c.curve_flat,
    ),
    (
        "kbf_sched_load_profile_cpu_total",
        "Of those, charged by curve in CPU",
        |c| c.profile_cpu,
    ),
    (
        "kbf_sched_load_profile_memory_total",
        "Of those, charged by curve in memory",
        |c| c.profile_memory,
    ),
    (
        "kbf_sched_load_blocked_total",
        "Queued requests, once per request per round, that had a node with room apart from the predicate and found none it admitted",
        |c| c.blocked,
    ),
    (
        "kbf_sched_load_cold_deferred_total",
        "Of those, requests whose only obstacle was the cold cap",
        |c| c.cold_deferred,
    ),
    (
        "kbf_sched_load_reservations_total",
        "Head reservations taken or moved",
        |c| c.reservations,
    ),
    (
        "kbf_sched_load_score_evaluations_total",
        "Slot-resource evaluations of the placement score",
        |c| c.score_evaluations,
    ),
    (
        "kbf_sched_load_scored_total",
        "Placements chosen by the score among two or more nodes",
        |c| c.scored,
    ),
    (
        "kbf_sched_load_chose_slack_total",
        "Of those, placements on a node predicted above its capacity",
        |c| c.chose_slack,
    ),
    (
        "kbf_sched_load_chose_distrusted_total",
        "Of those, placements on a distrusted node",
        |c| c.chose_distrusted,
    ),
    (
        "kbf_sched_load_chose_concentrated_total",
        "Of those, placements on a node concentrated on the lease's key",
        |c| c.chose_concentrated,
    ),
    (
        "kbf_sched_load_chose_by_tie_break_total",
        "Of those, placements where the best two were equal in the first five keys and a tie-break decided",
        |c| c.chose_by_tie_break,
    ),
];

/// The gauge of [`LoadCounters::fallback_cpu_limited`].
const FALLBACK: (&str, &str) = (
    "kbf_sched_load_fallback_cpu_limited_nodes",
    "Live nodes charged by prediction because their newest reading says the actions cgroup has a CPU limit, while --placement-load is on (not a running total)",
);

/// Appends the scheduler's counters to `out`.
fn render_counters(out: &mut String, counters: &LoadCounters) {
    for (name, help, count) in COUNTS {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} counter\n{name} {}",
            count(counters)
        );
    }
    let (name, help) = FALLBACK;
    let _ = writeln!(
        out,
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {}",
        counters.fallback_cpu_limited
    );
}

/// Appends what the scheduler counted, from `farm`, to `out`.
pub(crate) fn render_farm(out: &mut String, farm: &crate::farm::Farm) {
    render_counters(out, &farm.load_counters());
}

#[cfg(test)]
mod tests;
