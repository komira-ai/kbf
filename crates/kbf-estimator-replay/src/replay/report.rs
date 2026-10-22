//! The text of a replay: what it measured, what it could not.

use std::fmt::Write;

use super::sim::ArmRun;
use super::stats::{Rate, Section, Window, fidelity, section};
use super::trace::Trace;
use super::{Arm, Params};

/// What the replay cannot see. Printed with every report.
pub const LIMITS: [&str; 8] = [
    "Memory only: a run is killed iff its modelled need exceeds the limit of its booking (1.5 x booking + 512 MiB). The swap-kill and reclaim paths of the container driver are not modelled.",
    "A killed run's recorded peak stops at its limit, so a need above it is taken as one byte more; a run under 4 GiB shows a peak of at most 6.5 GiB. Heavy tails are invisible.",
    "Recorded wall hides contention: attempts run for the time they took on the recorded farm, whatever the replay's farm looks like.",
    "The floor is applied when an operation arrives; the design applies it again at every placement round, so an operation queued across a kill event is not raised here.",
    "Queue wait is the scheduler's on the recorded nodes as they are now (capacity from the export), alone: no other load, no node churn.",
    "Only Linux attempts with a shape, a node capacity and an own-limit or no memory kill are replayed; the funnel counts the rest.",
    "A hint is taken to be an operation whose first booking is not the default rung.",
    "Day 1 starts with no learned state. Phase 1 has no samples-based lowering: the third arm is an upper bound for it, not a proposal. Only a concurrent farm holdout supports a claim.",
];

/// A fixed-width number for a table.
fn ms(v: u64) -> String {
    format!("{v} ms")
}

fn rate(r: Rate) -> String {
    let (lo, hi) = r.wilson_per_1000();
    format!(
        "{}/{} = {:.2}/1000 [{:.2}, {:.2}]",
        r.hits,
        r.of,
        r.per_1000(),
        lo,
        hi
    )
}

fn write_section(out: &mut String, name: &str, s: &Section) {
    let _ = writeln!(
        out,
        "  {name}: {} operations ({} joined, {} refused)",
        s.ops, s.joined, s.refused
    );
    let _ = writeln!(
        out,
        "    own-limit kills per 1000 attempts: {}",
        rate(s.kills)
    );
    let _ = writeln!(
        out,
        "    ladder climbs per 1000 operations: {}",
        rate(s.climbs)
    );
    match s.waits {
        Some(w) => {
            let _ = writeln!(
                out,
                "    queue wait p50 {} p95 {} p99 {}",
                ms(w.p50),
                ms(w.p95),
                ms(w.p99)
            );
        }
        None => {
            let _ = writeln!(out, "    queue wait: no attempts");
        }
    }
    let drains: Vec<String> = s
        .drain_ms
        .iter()
        .map(|(d, v)| format!("day {d}: {}", ms(*v)))
        .collect();
    let _ = writeln!(
        out,
        "    backlog drain after each day: {}",
        drains.join(", ")
    );
    let cold: u64 = s.cold.values().sum();
    let _ = writeln!(
        out,
        "    resolved at L1 {} L2 {} cold {} (no estimate) of {}",
        s.l1, s.l2, cold, s.ops
    );
    if !s.cold.is_empty() {
        let why: Vec<String> = s.cold.iter().map(|(k, v)| format!("{k} {v}")).collect();
        let _ = writeln!(out, "    cold by reason: {}", why.join(", "));
    }
    let _ = writeln!(
        out,
        "    mixed-key operations: {}; first booking raised by the arm: {}",
        s.mixed, s.raised
    );
}

/// The report for `runs` (one per arm) over `trace`.
pub fn render(trace: &Trace, params: &Params, runs: &[ArmRun]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "kbf estimator replay (memory only)");
    let _ = writeln!(
        out,
        "parameters (frozen before scoring): ladder {:?} GiB, train days 1-{}, scored days {}-{}, chain gap {} s, heartbeat {} s",
        params.ladder_gib,
        params.train_days,
        params.train_days + 1,
        params.train_days + params.score_days,
        params.chain_gap_us / 1_000_000,
        params.heartbeat_ms / 1_000
    );
    let f = &trace.funnel;
    let _ = writeln!(
        out,
        "rows: {} read, {} kept in {} operations ({} recorded kills over {} attempts); {} nodes",
        f.rows,
        f.kept,
        trace.recorded.ops,
        trace.recorded.kills,
        trace.recorded.attempts,
        trace.nodes.len()
    );
    let dropped: Vec<String> = f.dropped.iter().map(|(k, v)| format!("{k} {v}")).collect();
    let _ = writeln!(
        out,
        "rows dropped: {}",
        if dropped.is_empty() {
            "none".to_owned()
        } else {
            dropped.join(", ")
        }
    );
    let _ = writeln!(
        out,
        "operations with no shape key: {}; completed with no recorded peak: {}",
        f.no_shape, f.no_peak
    );
    let train = Window {
        first: 1,
        last: params.train_days,
    };
    let scored = Window {
        first: params.train_days + 1,
        last: params.train_days + params.score_days,
    };
    let cold_start = Window { first: 1, last: 1 };
    let mut climbs = Vec::new();
    for run in runs.iter().filter(|r| r.arm == Arm::Null) {
        let fid = fidelity(trace, run);
        let _ = writeln!(
            out,
            "null-arm fidelity: {} own-limit kills simulated, {} recorded ({})",
            fid.simulated,
            fid.recorded,
            if fid.within_percent(1) {
                "within 1%"
            } else {
                "NOT within 1%: read the arms with care"
            }
        );
    }
    for run in runs {
        let _ = writeln!(out, "\narm: {}", run.arm.name());
        write_section(&mut out, "cold start (day 1)", &section(run, cold_start));
        write_section(&mut out, "training window", &section(run, train));
        let scored_section = section(run, scored);
        write_section(&mut out, "SCORED window", &scored_section);
        climbs.push((run.arm, scored_section.kills.hits));
        let s = &run.samples;
        let refused: Vec<String> = s.refused.iter().map(|(k, v)| format!("{k} {v}")).collect();
        let _ = writeln!(
            out,
            "  attempts shown to the estimator: {} taken as memory samples, refused [{}], {} of operations with no shape key",
            s.taken,
            refused.join(", "),
            s.no_key
        );
    }
    if let Some((_, null)) = climbs.iter().find(|(a, _)| *a == Arm::Null) {
        for (arm, kills) in climbs.iter().filter(|(a, _)| *a != Arm::Null) {
            let _ = writeln!(
                out,
                "\nladder re-climbs avoided on scored days, {}: {} (null {} kills, arm {})",
                arm.name(),
                i128::from(*null) - i128::from(*kills),
                null,
                kills
            );
        }
    }
    let _ = writeln!(out, "\nlimits:");
    for limit in LIMITS {
        let _ = writeln!(out, "  - {limit}");
    }
    out
}
