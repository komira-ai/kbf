//! `kbf_sched_load_*` and `kbf_node_load_*` on `GET /metrics`: what the scheduler has counted
//! of the node-load readings it was handed and of placement by load
//! ([`crate::farm::Farm::load_counters`]), and each node's load as its view shows it
//! ([`crate::farm::Farm::node_loads`], `docs/api.md`, "Node load"), so that placement by
//! measured load can be watched before and after `--placement-load` is turned on.
//!
//! The scheduler's counts are written on every scrape, 0 included: a scheduler that has
//! counted nothing is a fact. A node's gauges are written as the view shows them: a value the
//! view leaves out (`measured_cpu_millicores` is `null` with fewer than two windows and when
//! stale; the two telemetry counters when the daemon could not read them) has no sample, never
//! a 0, and a family no node has a sample of is not written. One sample per node that has a
//! load, labelled `node` as the pack families are, so the series are as many as there are
//! nodes. A node that has gone keeps its sample, stale, until the server restarts, as the
//! node view and the pack families keep it.

use std::fmt::Write as _;

use kbf_sched::LoadCounters;
use kbf_types::WorkerId;

use crate::fleet::LoadView;
use crate::pack_daemon_metrics::label_value;

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
pub(crate) fn render_counters(out: &mut String, counters: &LoadCounters) {
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

/// The kind of a node family.
#[derive(Clone, Copy)]
enum Kind {
    Gauge,
    Counter,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Self::Gauge => "gauge",
            Self::Counter => "counter",
        }
    }
}

/// A sample's value as it is written.
#[derive(Clone, Copy)]
enum Num {
    /// A whole number, as the view has it.
    Whole(u64),
    /// Microseconds, written as seconds with six decimals (exact, no rounding).
    Micros(u64),
}

impl std::fmt::Display for Num {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::Whole(v) => write!(f, "{v}"),
            Self::Micros(v) => write!(f, "{}.{:06}", v / 1_000_000, v % 1_000_000),
        }
    }
}

/// One node family: its name, kind and `# HELP` text, and the value a node's view gives it
/// (`None` when the view leaves it out).
type Gauge = (
    &'static str,
    Kind,
    &'static str,
    fn(&LoadView) -> Option<Num>,
);

/// The families with one sample per node, in the order they are written.
const NODE_FAMILIES: [Gauge; 9] = [
    (
        "kbf_node_load_measured_cpu_millicores",
        Kind::Gauge,
        "The CPU a node's actions used, in thousandths of a CPU: the larger of its last two windows; absent with fewer than two windows and when stale",
        |v| v.measured_cpu_millicores.map(Num::Whole),
    ),
    (
        "kbf_node_load_memory_working_set_bytes",
        Kind::Gauge,
        "memory.current less the inactive file pages of a node's actions cgroup at its last accepted reading, in bytes",
        |v| Some(Num::Whole(v.memory_working_set_bytes)),
    ),
    (
        "kbf_node_load_memory_active_file_bytes",
        Kind::Gauge,
        "The active file pages of a node's actions cgroup at its last accepted reading, in bytes",
        |v| Some(Num::Whole(v.memory_active_file_bytes)),
    ),
    (
        "kbf_node_load_admission_working_set_bytes",
        Kind::Gauge,
        "A node's working set less the active file pages beyond a quarter of its memory: what an admission would count, in bytes",
        |v| Some(Num::Whole(v.admission_working_set_bytes)),
    ),
    (
        "kbf_node_load_stale",
        Kind::Gauge,
        "1 when no reading of a node was accepted for three windows, else 0",
        |v| Some(Num::Whole(u64::from(v.stale))),
    ),
    (
        "kbf_node_load_actions_cpu_limited",
        Kind::Gauge,
        "1 when a node's actions cgroup has a CPU quota, else 0",
        |v| Some(Num::Whole(u64::from(v.actions_cpu_limited))),
    ),
    (
        "kbf_node_load_cpu_throttled_seconds_total",
        Kind::Counter,
        "Cumulative CPU throttled time of a node's actions cgroup, in seconds, as its daemon read it (it can fall if the cgroup is recreated within a session); absent when it could not",
        |v| v.cpu_throttled_usec.map(Num::Micros),
    ),
    (
        "kbf_node_load_cpu_pressure_some_seconds_total",
        Kind::Counter,
        "Cumulative CPU pressure (some total) of a node's actions cgroup, in seconds, as its daemon read it (it can fall if the cgroup is recreated within a session); absent when it could not",
        |v| v.cpu_pressure_some_total_usec.map(Num::Micros),
    ),
    (
        "kbf_node_load_refused_minutes_total",
        Kind::Counter,
        "Minute records carried by readings of a node that were not accepted, since its daemon process began",
        |v| Some(Num::Whole(v.lists.refused_minutes)),
    ),
];

/// The list counters of a node's readings, since its daemon process began.
const LISTS: [Gauge; 2] = [
    (
        "kbf_node_load_clocks_total",
        Kind::Counter,
        "Lease clock records of a node's accepted readings that were kept, since its daemon process began",
        |v| Some(Num::Whole(v.lists.clocks)),
    ),
    (
        "kbf_node_load_minutes_total",
        Kind::Counter,
        "Lease minute records of a node's accepted readings that were kept, since its daemon process began",
        |v| Some(Num::Whole(v.lists.minutes)),
    ),
];

/// The name and help of the family of dropped records, which has a `reason` as well as a `node`.
const DROPPED: (&str, &str) = (
    "kbf_node_load_dropped_total",
    "Records of a node's reading lists left out, by reason, since its daemon process began",
);

/// One family: header, then a sample for each node that has a value. Nothing when no node has.
fn family(out: &mut String, (name, kind, help, value): Gauge, nodes: &[(WorkerId, LoadView)]) {
    let samples: Vec<_> = nodes
        .iter()
        .filter_map(|(node, view)| value(view).map(|v| (label_value(node.as_str()), v)))
        .collect();
    if samples.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {}", kind.word());
    for (node, v) in samples {
        let _ = writeln!(out, "{name}{{node=\"{node}\"}} {v}");
    }
}

/// Appends the families for `nodes`, each node's load: nothing when `nodes` is empty.
pub(crate) fn render_nodes(out: &mut String, nodes: &[(WorkerId, LoadView)]) {
    for gauge in NODE_FAMILIES.into_iter().chain(LISTS) {
        family(out, gauge, nodes);
    }
    if nodes.is_empty() {
        return;
    }
    let (name, help) = DROPPED;
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
    for (node, view) in nodes {
        let node = label_value(node.as_str());
        let dropped = &view.lists.dropped;
        for (reason, count) in [
            ("invalid", dropped.invalid),
            ("duplicate", dropped.duplicate),
            ("over_cap", dropped.over_cap),
        ] {
            let _ = writeln!(out, "{name}{{node=\"{node}\",reason=\"{reason}\"}} {count}");
        }
    }
}

/// Appends what the scheduler counted and each node's load, from `farm`, to `out`.
pub(crate) fn render_farm(out: &mut String, farm: &crate::farm::Farm) {
    render_counters(out, &farm.load_counters());
    render_nodes(out, &farm.node_loads());
}

#[cfg(test)]
mod tests;
