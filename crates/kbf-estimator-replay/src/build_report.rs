//! The per-build rows of the two-arm table and the per-build keys of its JSON
//! ([`crate::builds`] has the definitions).

use serde_json::{Value, json};

use crate::builds::{BuildStats, Spread};
use crate::compare::{count_row, delta, duration_row};

/// Appends the per-build rows of the table: counts, then wall and queue wait percentiles. An
/// arm that names no build prints `-` for the percentiles, as an absent number does.
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
        "builds with no end (no wall)",
        a.unfinished,
        b.unfinished,
    );
    for (label, get) in [
        (
            "build wall",
            (|s: &BuildStats| s.wall) as fn(&BuildStats) -> Spread,
        ),
        ("build queue wait", |s: &BuildStats| s.queue_wait),
    ] {
        duration_row(out, &format!("{label} p50 (s)"), get(a).p50, get(b).p50);
        duration_row(out, &format!("{label} p90 (s)"), get(a).p90, get(b).p90);
    }
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
        "wall_samples": arm.wall_samples,
        "wall_us": spread_json(arm.wall),
        "queue_wait_samples": arm.queue_wait_samples,
        "queue_wait_us": spread_json(arm.queue_wait),
    })
}

fn spread_delta(a: Spread, b: Spread) -> Value {
    json!({"p50": delta(a.p50, b.p50), "p90": delta(a.p90, b.p90)})
}

/// `b - a` of the per-build percentiles, `null` where either arm has none.
pub(crate) fn difference_json(a: &BuildStats, b: &BuildStats) -> Value {
    json!({
        "wall_us": spread_delta(a.wall, b.wall),
        "queue_wait_us": spread_delta(a.queue_wait, b.queue_wait),
    })
}
