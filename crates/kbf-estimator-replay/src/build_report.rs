//! The per-build rows of the two-arm table and the per-build keys of its JSON
//! ([`crate::builds`] has the definitions).

use serde_json::{Value, json};

use crate::builds::{BuildStats, Spread};
use crate::compare::{count_row, delta, duration_row};

/// What the span is, printed with the per-build rows.
pub(crate) const SPAN_NOTE: &str =
    "build action span (server): excludes the client's own analysis and local actions";

/// Appends the per-build rows of the table: counts, the server's build action span and the
/// queue time total (attempt-seconds) at p50 and p90 over builds, and the median over builds
/// of each build's own attempt queue wait p50 and p90. An arm that names no build prints `-`
/// for the numbers, as an absent number does.
pub(crate) fn rows(out: &mut String, a: &BuildStats, b: &BuildStats) {
    count_row(out, "builds", a.runs.len() as u64, b.runs.len() as u64);
    count_row(
        out,
        "build rows with no id",
        a.rows_without_id,
        b.rows_without_id,
    );
    count_row(
        out,
        "builds with no end (no span)",
        a.unfinished,
        b.unfinished,
    );
    if [a, b].iter().any(|arm| arm.recorded()) {
        out.push_str(SPAN_NOTE);
        out.push('\n');
    }
    type Get = fn(&BuildStats) -> Spread;
    let spreads: [(&str, &str, Get); 2] = [
        ("build action span", "(s)", |s| s.action_span),
        ("queue time total", "(attempt-s)", |s| s.queue_time_total),
    ];
    for (label, unit, get) in spreads {
        duration_row(out, &format!("{label} p50 {unit}"), get(a).p50, get(b).p50);
        duration_row(out, &format!("{label} p90 {unit}"), get(a).p90, get(b).p90);
    }
    duration_row(
        out,
        "attempt wait p50 in a build (s)",
        a.attempt_wait_p50.p50,
        b.attempt_wait_p50.p50,
    );
    duration_row(
        out,
        "attempt wait p90 in a build (s)",
        a.attempt_wait_p90.p50,
        b.attempt_wait_p90.p50,
    );
}

fn spread_json(s: Spread) -> Value {
    json!({"p50": s.p50, "p90": s.p90})
}

/// One arm's per-build numbers, times in microseconds.
pub(crate) fn arm_json(arm: &BuildStats) -> Value {
    json!({
        "count": arm.runs.len(),
        "rows_without_id": arm.rows_without_id,
        "unfinished": arm.unfinished,
        "out_of_order": arm.out_of_order,
        "action_span_samples": arm.span_samples,
        "action_span_us": spread_json(arm.action_span),
        "queue_time_total_samples": arm.queue_total_samples,
        "queue_time_total_us": spread_json(arm.queue_time_total),
        "attempt_queue_wait_p50_us": spread_json(arm.attempt_wait_p50),
        "attempt_queue_wait_p90_us": spread_json(arm.attempt_wait_p90),
    })
}

fn spread_delta(a: Spread, b: Spread) -> Value {
    json!({"p50": delta(a.p50, b.p50), "p90": delta(a.p90, b.p90)})
}

/// `b - a` of the per-build percentiles, `null` where either arm has none.
pub(crate) fn difference_json(a: &BuildStats, b: &BuildStats) -> Value {
    json!({
        "action_span_us": spread_delta(a.action_span, b.action_span),
        "queue_time_total_us": spread_delta(a.queue_time_total, b.queue_time_total),
        "attempt_queue_wait_p50_us": spread_delta(a.attempt_wait_p50, b.attempt_wait_p50),
        "attempt_queue_wait_p90_us": spread_delta(a.attempt_wait_p90, b.attempt_wait_p90),
    })
}
